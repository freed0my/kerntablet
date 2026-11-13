//! Plugins: separately installable HID-BPF filters (see `bpf/kerntablet-plugin.h`).
//!
//! A plugin package is a directory with `plugin.toml` (the manifest) and a
//! compiled `.bpf.o`. The manifest describes the settings shown in the window
//! and how they become the bytes of the plugin's config map: a list of fields
//! with C layout, each computed by an expression (`expr.rs`) in userspace.
//!
//! Installed plugins live in a root-owned directory (`PLUGIN_DIR`); the
//! loader attaches every installed plugin between the core's two programs.
//! Plugins that are not installed cost nothing.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::expr::{self, Expr, Vars};
use crate::tablet::TabletSpec;

pub const PLUGIN_ABI: u32 = 1;
pub const KT_PLUGIN_MAGIC: u32 = 0x3150_544b;
/// Size of `struct kt_pipe` (checked against the plugin object by the loader).
pub const KT_PIPE_SIZE: u32 = 48;
pub const MANIFEST: &str = "plugin.toml";
/// Installed plugins, one directory per id. Root-owned: whatever is here gets
/// loaded into the kernel by the root loader.
pub const PLUGIN_DIR: &str = "/usr/local/lib/kerntablet/plugins";
/// Name of the config map inside a plugin object.
pub const CONFIG_MAP: &str = "kt_plugin_config";
pub const PIPE_MAP: &str = "kt_pipe";
pub const DEFAULT_REPOSITORY: &str = "https://raw.githubusercontent.com/freed0my/kerntablet/main/repo/index.json";

/// Overridable for development and tests.
pub fn plugin_dir() -> PathBuf {
    std::env::var_os("KERNTABLET_PLUGIN_DIR").map_or_else(|| PathBuf::from(PLUGIN_DIR), PathBuf::from)
}

// ---------------------------------------------------------------- manifest

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub abi: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    /// File name of the compiled object next to the manifest.
    pub object: String,
    /// Position in the chain; lower runs first.
    #[serde(default = "default_order")]
    pub order: i32,
    /// `Path` of the equivalent OpenTabletDriver filter, for importing settings.
    #[serde(default)]
    pub otd_filter: Option<String>,
    /// `name = expr` evaluated once, in order, before the fields.
    #[serde(default)]
    pub lets: Vec<String>,
    #[serde(default, rename = "param")]
    pub params: Vec<Param>,
    #[serde(default, rename = "field")]
    pub fields: Vec<Field>,
}

fn default_order() -> i32 {
    100
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    #[default]
    Float,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub key: String,
    pub label: String,
    #[serde(default, rename = "type")]
    pub kind: ParamKind,
    pub default: Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub tip: Option<String>,
    /// Property name in the OTD filter.
    #[serde(default)]
    pub otd: Option<String>,
    /// Key of a bool param that has to be on for this one to matter (UI only).
    #[serde(default)]
    pub enabled_if: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
}

impl FieldType {
    pub fn size(self) -> usize {
        match self {
            FieldType::U8 | FieldType::I8 => 1,
            FieldType::U16 | FieldType::I16 => 2,
            FieldType::U32 | FieldType::I32 => 4,
            FieldType::U64 | FieldType::I64 => 8,
        }
    }

    /// Rounds and saturates; NaN becomes 0.
    fn encode(self, v: f64, out: &mut [u8]) {
        let v = if v.is_nan() { 0.0 } else { v.round() };
        // `as` saturates float -> int conversions.
        match self {
            FieldType::U8 => out.copy_from_slice(&(v as u8).to_le_bytes()),
            FieldType::I8 => out.copy_from_slice(&(v as i8).to_le_bytes()),
            FieldType::U16 => out.copy_from_slice(&(v as u16).to_le_bytes()),
            FieldType::I16 => out.copy_from_slice(&(v as i16).to_le_bytes()),
            FieldType::U32 => out.copy_from_slice(&(v as u32).to_le_bytes()),
            FieldType::I32 => out.copy_from_slice(&(v as i32).to_le_bytes()),
            FieldType::U64 => out.copy_from_slice(&(v as u64).to_le_bytes()),
            FieldType::I64 => out.copy_from_slice(&(v as i64).to_le_bytes()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LutKind {
    /// `KT_LUT_LEN` entries indexed by squared distance (`kt_lut()`), `d` in mm.
    Dist,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    #[serde(default)]
    pub lut: Option<LutKind>,
    /// Evaluated per entry (tables) or once, after the global `lets`.
    #[serde(default)]
    pub lets: Vec<String>,
    pub expr: String,
}

impl Field {
    fn count(&self) -> usize {
        match self.lut {
            Some(LutKind::Dist) => KT_LUT_LEN,
            None => 1,
        }
    }
}

/// A setting value in `plugins.toml` / a manifest default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Num(f64),
}

impl Value {
    pub fn as_f64(self) -> f64 {
        match self {
            Value::Bool(b) => b as u8 as f64,
            Value::Num(n) => n,
        }
    }

    pub fn as_bool(self) -> bool {
        match self {
            Value::Bool(b) => b,
            Value::Num(n) => n != 0.0,
        }
    }
}

// ---------------------------------------------------------------- lookup tables

/// Mirrors of the helpers in `kerntablet-plugin.h`.
pub const KT_LUT_LEN: usize = 288;
pub const KT_LUT_QMAX: u64 = (1 << 36) - 1;

/// Squared length of a Q20 vector in (1/16 unit)^2 — `kt_dist2`.
pub fn dist2(dx: i64, dy: i64) -> u64 {
    let (dx, dy) = (dx >> 16, dy >> 16);
    (dx * dx + dy * dy) as u64
}

/// Table index of a squared distance and the bucket's shift (0 = exact entry).
pub fn lut_index(q: u64) -> (usize, u32) {
    let q = q.min(KT_LUT_QMAX);
    if q < 16 {
        return (q as usize, 0);
    }
    let e = 63 - q.leading_zeros();
    let sh = e - 3;
    (16 + (e as usize - 4) * 8 + ((q >> sh) & 7) as usize, sh)
}

/// The squared distance a table entry is sampled at (inverse of `lut_index`).
pub fn lut_q(idx: usize) -> u64 {
    if idx < 16 {
        return idx as u64;
    }
    let e = (idx - 16) / 8 + 4;
    let sub = ((idx - 16) % 8) as u64;
    (8 + sub) << (e - 3)
}

/// Interpolated lookup — `kt_lut`.
pub fn lut(table: &[i32], q: u64) -> i64 {
    let q = q.min(KT_LUT_QMAX);
    let (idx, sh) = lut_index(q);
    let idx = idx.min(KT_LUT_LEN - 2);
    let a = table[idx] as i64;
    if sh == 0 {
        return a;
    }
    let b = table[idx + 1] as i64;
    let frac = (q & ((1u64 << sh) - 1)) as i64;
    a + (((b - a) * frac) >> sh)
}

// ---------------------------------------------------------------- compiled plugin

type Lets = Vec<(String, Expr)>;

#[derive(Debug, Clone)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
    lets: Lets,
    fields: Vec<(Lets, Expr)>,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !id.starts_with('-')
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && !name.starts_with('.')
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

impl Plugin {
    pub fn load(dir: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(dir.join(MANIFEST))?;
        Self::parse(&text, dir)
    }

    pub fn parse(text: &str, dir: &Path) -> io::Result<Self> {
        let m: Manifest = toml::from_str(text).map_err(|e| bad(format!("{MANIFEST}: {e}")))?;
        let ctx = |e: String| bad(format!("plugin {}: {e}", m.id));
        if !valid_id(&m.id) {
            return Err(bad(format!("bad plugin id \"{}\" (a-z, 0-9, -)", m.id)));
        }
        if m.abi != PLUGIN_ABI {
            return Err(ctx(format!("needs plugin ABI {}, this kerntablet has {PLUGIN_ABI}", m.abi)));
        }
        if !valid_file_name(&m.object) {
            return Err(ctx(format!("bad object file name \"{}\"", m.object)));
        }
        let mut known: HashSet<String> =
            ["mm_per_unit", "max_x", "max_y", "width_mm", "height_mm", "max_pressure"].map(String::from).into();
        for p in &m.params {
            if !p.key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || !known.insert(p.key.clone()) {
                return Err(ctx(format!("bad or duplicate param key \"{}\"", p.key)));
            }
            let ok = match p.kind {
                ParamKind::Bool => matches!(p.default, Value::Bool(_)),
                ParamKind::Float => matches!(p.default, Value::Num(_)),
            };
            if !ok {
                return Err(ctx(format!("default of \"{}\" does not match its type", p.key)));
            }
        }
        for p in &m.params {
            if let Some(dep) = &p.enabled_if {
                if !m.params.iter().any(|q| &q.key == dep && q.kind == ParamKind::Bool) {
                    return Err(ctx(format!("enabled_if of \"{}\" is not a bool param", p.key)));
                }
            }
        }
        let parse_lets = |list: &[String], known: &mut HashSet<String>| -> io::Result<Lets> {
            let mut out = Vec::new();
            for l in list {
                let (name, e) = expr::parse_let(l).map_err(|e| ctx(e.0))?;
                check_vars(&e, known).map_err(ctx)?;
                known.insert(name.clone());
                out.push((name, e));
            }
            Ok(out)
        };
        let lets = parse_lets(&m.lets, &mut known)?;
        let mut fields = Vec::new();
        for f in &m.fields {
            let mut scope = known.clone();
            if f.lut.is_some() {
                scope.insert("d".into());
            }
            let fl = parse_lets(&f.lets, &mut scope)?;
            let e = expr::parse(&f.expr).map_err(|e| ctx(format!("field {}: {e}", f.name)))?;
            check_vars(&e, &scope).map_err(|e| ctx(format!("field {}: {e}", f.name)))?;
            fields.push((fl, e));
        }
        Ok(Self { manifest: m, dir: dir.to_path_buf(), lets, fields })
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn object_path(&self) -> PathBuf {
        self.dir.join(&self.manifest.object)
    }

    /// Size of the config map value: header + fields with C alignment.
    pub fn config_size(&self) -> usize {
        self.layout().1
    }

    fn layout(&self) -> (Vec<usize>, usize) {
        let mut off: usize = 8;
        let mut align: usize = 4;
        let mut offs = Vec::new();
        for f in &self.manifest.fields {
            let a = f.ty.size();
            align = align.max(a);
            off = off.div_ceil(a) * a;
            offs.push(off);
            off += a * f.count();
        }
        (offs, off.div_ceil(align) * align)
    }

    /// Settings with defaults filled in and values clamped to the manifest.
    pub fn resolve(&self, entry: Option<&PluginEntry>) -> PluginEntry {
        let mut out = PluginEntry { enabled: entry.is_some_and(|e| e.enabled), values: BTreeMap::new() };
        for p in &self.manifest.params {
            let v = entry.and_then(|e| e.values.get(&p.key)).copied().unwrap_or(p.default);
            let v = match p.kind {
                ParamKind::Bool => Value::Bool(v.as_bool()),
                ParamKind::Float => {
                    let mut n = v.as_f64();
                    if !n.is_finite() {
                        n = p.default.as_f64();
                    }
                    if let Some(lo) = p.min {
                        n = n.max(lo);
                    }
                    if let Some(hi) = p.max {
                        n = n.min(hi);
                    }
                    Value::Num(n)
                }
            };
            out.values.insert(p.key.clone(), v);
        }
        out
    }

    /// The config map value for these settings on this tablet model.
    pub fn config_bytes(&self, entry: &PluginEntry, spec: &TabletSpec) -> Result<Vec<u8>, String> {
        let entry = self.resolve(Some(entry));
        let mut vars = Vars::new();
        let mm_per_unit = spec.width_mm / spec.max_x as f64;
        for (k, v) in [
            ("mm_per_unit", mm_per_unit),
            ("max_x", spec.max_x as f64),
            ("max_y", spec.max_y as f64),
            ("width_mm", spec.width_mm),
            ("height_mm", spec.height_mm),
            ("max_pressure", 2047.0),
        ] {
            vars.insert(k.into(), v);
        }
        for (k, v) in &entry.values {
            vars.insert(k.clone(), v.as_f64());
        }
        let err = |e: expr::Error| format!("plugin {}: {e}", self.id());
        for (name, e) in &self.lets {
            let v = e.eval(&vars).map_err(err)?;
            vars.insert(name.clone(), v);
        }

        let (offs, size) = self.layout();
        let mut out = vec![0u8; size];
        out[0..4].copy_from_slice(&KT_PLUGIN_MAGIC.to_le_bytes());
        out[4..8].copy_from_slice(&(entry.enabled as u32).to_le_bytes());
        for ((f, (flets, e)), off) in self.manifest.fields.iter().zip(&self.fields).zip(offs) {
            let sz = f.ty.size();
            for i in 0..f.count() {
                let mut v = vars.clone();
                if f.lut == Some(LutKind::Dist) {
                    v.insert("d".into(), (lut_q(i) as f64).sqrt() / 16.0 * mm_per_unit);
                }
                for (name, le) in flets {
                    let x = le.eval(&v).map_err(err)?;
                    v.insert(name.clone(), x);
                }
                let x = e.eval(&v).map_err(err)?;
                let at = off + i * sz;
                f.ty.encode(x, &mut out[at..at + sz]);
            }
        }
        Ok(out)
    }

    /// Settings from the matching filter of an OpenTabletDriver profile.
    pub fn import_otd(&self, settings_json: &str) -> Option<PluginEntry> {
        let path = self.manifest.otd_filter.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(settings_json).ok()?;
        let f = v
            .get("Profiles")?
            .get(0)?
            .get("Filters")?
            .as_array()?
            .iter()
            .find(|f| f.get("Path").and_then(|p| p.as_str()) == Some(path))?;
        let mut e = PluginEntry {
            enabled: f.get("Enable").and_then(|e| e.as_bool()).unwrap_or(false),
            values: BTreeMap::new(),
        };
        for s in f.get("Settings").and_then(|s| s.as_array()).into_iter().flatten() {
            let (Some(name), Some(val)) = (s.get("Property").and_then(|p| p.as_str()), s.get("Value")) else {
                continue;
            };
            let Some(p) = self.manifest.params.iter().find(|p| p.otd.as_deref() == Some(name)) else {
                continue;
            };
            let v = match (p.kind, val) {
                (ParamKind::Bool, serde_json::Value::Bool(b)) => Value::Bool(*b),
                (ParamKind::Float, v) if v.is_number() => Value::Num(v.as_f64()?),
                _ => continue,
            };
            e.values.insert(p.key.clone(), v);
        }
        Some(self.resolve(Some(&e)))
    }
}

fn check_vars(e: &Expr, known: &HashSet<String>) -> Result<(), String> {
    let mut vs = Vec::new();
    e.vars(&mut vs);
    match vs.into_iter().find(|v| !known.contains(v)) {
        Some(v) => Err(format!("unknown variable \"{v}\"")),
        None => Ok(()),
    }
}

/// Installed plugins in chain order. Broken ones are reported, not loaded.
pub fn installed() -> (Vec<Plugin>, Vec<String>) {
    let mut ok = Vec::new();
    let mut errors = Vec::new();
    let Ok(rd) = fs::read_dir(plugin_dir()) else { return (ok, errors) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !valid_id(&name) || !e.path().is_dir() {
            continue;
        }
        match Plugin::load(&e.path()) {
            Ok(p) if p.id() == name => ok.push(p),
            Ok(p) => errors.push(format!("{name}: manifest id is \"{}\"", p.id())),
            Err(err) => errors.push(format!("{name}: {err}")),
        }
    }
    ok.sort_by(|a, b| (a.manifest.order, a.id()).cmp(&(b.manifest.order, b.id())));
    (ok, errors)
}

// ---------------------------------------------------------------- user settings

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PluginEntry {
    #[serde(default)]
    pub enabled: bool,
    #[serde(flatten)]
    pub values: BTreeMap<String, Value>,
}

/// `~/.config/kerntablet/plugins.toml`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PluginSettings {
    /// URL of the repository index; the default one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default)]
    pub plugin: BTreeMap<String, PluginEntry>,
}

impl PluginSettings {
    pub fn path() -> PathBuf {
        crate::config::config_home().join("kerntablet").join("plugins.toml")
    }

    pub fn load() -> io::Result<Self> {
        match fs::read_to_string(Self::path()) {
            Ok(t) => toml::from_str(&t).map_err(|e| bad(e.to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, toml::to_string_pretty(self).map_err(|e| bad(e.to_string()))?)
    }

    pub fn repository(&self) -> &str {
        self.repository.as_deref().filter(|r| !r.is_empty()).unwrap_or(DEFAULT_REPOSITORY)
    }

    /// Fills in every installed plugin with its defaults, clamps the rest.
    pub fn normalize(&mut self, plugins: &[Plugin]) {
        for p in plugins {
            let e = p.resolve(self.plugin.get(p.id()));
            self.plugin.insert(p.id().to_string(), e);
        }
    }
}

/// Last applied config of a plugin, re-loaded by the loader on hotplug/boot.
pub fn state_file(vid: u16, pid: u16, id: &str) -> PathBuf {
    PathBuf::from(crate::STATE_DIR).join(format!("{vid:04X}-{pid:04X}.{id}.plugin"))
}

// ---------------------------------------------------------------- repository

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub abi: u32,
    pub plugins: Vec<IndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    pub files: Vec<IndexFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexFile {
    pub name: String,
    pub sha256: String,
}

fn curl(url: &str) -> Result<Vec<u8>, String> {
    if !(url.starts_with("https://") || url.starts_with("file://")) {
        return Err(format!("{url}: only https:// and file:// repositories are supported"));
    }
    let out = Command::new("curl")
        .args(["-fsSL", "--proto", "=https,file", "--max-time", "30", "--max-filesize", "16000000", "--", url])
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(out.stdout)
}

pub fn fetch_index(url: &str) -> Result<Index, String> {
    let body = curl(url)?;
    let idx: Index = serde_json::from_slice(&body).map_err(|e| format!("{url}: bad index: {e}"))?;
    Ok(idx)
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn base_url(index_url: &str) -> &str {
    index_url.rsplit_once('/').map_or(index_url, |(base, _)| base)
}

/// Downloads a plugin into a fresh cache directory and checks every file
/// against the index. Returns the directory, ready for `kerntablet-loader
/// plugin-install`.
pub fn download(index_url: &str, entry: &IndexEntry) -> Result<PathBuf, String> {
    if !valid_id(&entry.id) {
        return Err(format!("bad plugin id \"{}\" in the index", entry.id));
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    let dir = cache.join("kerntablet").join("download").join(&entry.id);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    for f in &entry.files {
        if !valid_file_name(&f.name) {
            return Err(format!("bad file name \"{}\" in the index", f.name));
        }
        let url = format!("{}/{}/{}", base_url(index_url), entry.id, f.name);
        let data = curl(&url)?;
        let got = sha256_hex(&data);
        if !got.eq_ignore_ascii_case(&f.sha256) {
            let _ = fs::remove_dir_all(&dir);
            return Err(format!("{}: checksum mismatch (expected {}, got {got})", f.name, f.sha256));
        }
        fs::write(dir.join(&f.name), data).map_err(|e| format!("writing {}: {e}", f.name))?;
    }
    let p = Plugin::load(&dir).map_err(|e| e.to_string())?;
    if p.id() != entry.id {
        return Err(format!("the package says it is \"{}\", the index says \"{}\"", p.id(), entry.id));
    }
    if !p.object_path().exists() {
        return Err(format!("{} is missing from the package", p.manifest.object));
    }
    Ok(dir)
}

// Tests of the Devocub Antichatter port, which lives outside the public
// project; see build.rs.
#[cfg(all(test, kt_antichatter))]
#[path = "../../../plugins/devocub-antichatter/tests.rs"]
mod antichatter_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tablet::TABLETS;

    /// A small plugin using every manifest feature: params of both kinds,
    /// `lets`, plain fields and a distance table, an OTD mapping.
    const EXAMPLE: &str = r#"
id = "example"
name = "Example"
version = "1.2.0"
abi = 1
object = "example.bpf.o"
otd_filter = "Some.Otd.Filter"
lets = ["interval = 1000 / frequency"]

[[param]]
key = "latency"
label = "Latency"
default = 2.0
min = 0.0
max = 100.0
unit = "ms"
otd = "Latency"

[[param]]
key = "frequency"
label = "Frequency"
default = 1000.0
min = 10.0
max = 8000.0
otd = "Frequency"

[[param]]
key = "predict"
label = "Prediction"
type = "bool"
default = false
otd = "PredictionEnabled"

[[field]]
name = "tick_ns"
type = "u32"
expr = "round(interval * 1e6)"

[[field]]
name = "flags"
type = "u32"
expr = "predict"

[[field]]
name = "weight"
type = "i32"
lut = "dist"
lets = ["w = pow(d + 1, -latency)"]
expr = "q20(clamp(w, 0, 1))"
"#;

    fn example() -> Plugin {
        Plugin::parse(EXAMPLE, Path::new("/nonexistent")).unwrap()
    }

    #[test]
    fn lut_index_roundtrip() {
        for i in (0..KT_LUT_LEN).take_while(|&i| lut_q(i) <= KT_LUT_QMAX) {
            assert_eq!(lut_index(lut_q(i)), (i, if i < 16 { 0 } else { ((i - 16) / 8 + 1) as u32 }));
        }
        assert!(lut_index(KT_LUT_QMAX).0 < KT_LUT_LEN - 1);
    }

    #[test]
    fn layout_matches_the_c_struct() {
        // hdr(8) + tick_ns(4) + flags(4) + 288 * i32
        assert_eq!(example().config_size(), 8 + 4 + 4 + KT_LUT_LEN * 4);
    }

    #[test]
    fn fields_are_computed_from_the_params() {
        let p = example();
        let mut e = p.resolve(None);
        e.values.insert("predict".into(), Value::Bool(true));
        let b = p.config_bytes(&e, &TABLETS[0]).unwrap();
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!(u32_at(8), 1_000_000); // 1000 Hz → 1 ms
        assert_eq!(u32_at(12), 1);
        // d = 0 → (0 + 1)^-2 = 1.0 → Q20
        assert_eq!(u32_at(16), 1 << 20);
    }

    #[test]
    fn rejects_bad_manifests() {
        let base = "id = \"x\"\nname = \"X\"\nversion = \"1\"\nabi = 1\nobject = \"x.bpf.o\"\n";
        assert!(Plugin::parse(base, Path::new("/")).is_ok());
        assert!(Plugin::parse(&base.replace("abi = 1", "abi = 9"), Path::new("/")).is_err());
        assert!(Plugin::parse(&base.replace("\"x\"\nname", "\"../x\"\nname"), Path::new("/")).is_err());
        assert!(Plugin::parse(&base.replace("x.bpf.o", "../x.bpf.o"), Path::new("/")).is_err());
        let unknown_var = format!("{base}[[field]]\nname = \"a\"\ntype = \"u32\"\nexpr = \"nope + 1\"\n");
        assert!(Plugin::parse(&unknown_var, Path::new("/")).is_err());
        let d_outside_lut = format!("{base}[[field]]\nname = \"a\"\ntype = \"u32\"\nexpr = \"d\"\n");
        assert!(Plugin::parse(&d_outside_lut, Path::new("/")).is_err());
    }

    #[test]
    fn disabled_by_default_and_header() {
        let p = example();
        let e = p.resolve(None);
        assert!(!e.enabled);
        assert_eq!(e.values["latency"], Value::Num(2.0));
        let b = p.config_bytes(&e, &TABLETS[0]).unwrap();
        assert_eq!(&b[0..4], &KT_PLUGIN_MAGIC.to_le_bytes());
        assert_eq!(&b[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn imports_otd_filter_settings() {
        let json = r#"{"Profiles":[{"Filters":[{"Path":"Other.Filter","Settings":[],"Enable":true},
              {"Path":"Some.Otd.Filter","Enable":true,"Settings":[
                {"Property":"Latency","Value":6.0},{"Property":"Unknown","Value":1.4},
                {"Property":"PredictionEnabled","Value":true}]}]}]}"#;
        let e = example().import_otd(json).unwrap();
        assert!(e.enabled);
        assert_eq!(e.values["latency"], Value::Num(6.0));
        assert_eq!(e.values["predict"], Value::Bool(true));
        assert_eq!(e.values["frequency"], Value::Num(1000.0));
    }

    #[test]
    fn settings_roundtrip() {
        let mut s = PluginSettings::default();
        s.normalize(&[example()]);
        let text = toml::to_string_pretty(&s).unwrap();
        assert_eq!(toml::from_str::<PluginSettings>(&text).unwrap(), s);
    }

    #[test]
    fn all_tablets_are_isotropic() {
        // `d` is computed with the X scale only.
        for t in TABLETS {
            let (ux, uy) = (t.width_mm / t.max_x as f64, t.height_mm / t.max_y as f64);
            assert!((ux - uy).abs() < 1e-9, "{}", t.name);
        }
    }

    #[test]
    fn downloads_and_verifies_from_a_repository() {
        let tmp = std::env::temp_dir().join(format!("kt-dl-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        // A repository the way plugins/build.py writes one.
        let repo = tmp.join("repo");
        fs::create_dir_all(repo.join("example")).unwrap();
        let files = [("plugin.toml", EXAMPLE.as_bytes()), ("example.bpf.o", &b"not really an object"[..])];
        let mut listed = Vec::new();
        for (name, data) in files {
            fs::write(repo.join("example").join(name), data).unwrap();
            listed.push(serde_json::json!({ "name": name, "sha256": sha256_hex(data) }));
        }
        let index = serde_json::json!({ "abi": 1, "plugins": [{
            "id": "example", "name": "Example", "version": "1.2.0", "files": listed }] });
        fs::write(repo.join("index.json"), index.to_string()).unwrap();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));

        let url = format!("file://{}/index.json", repo.display());
        let idx = fetch_index(&url).unwrap();
        let entry = idx.plugins.iter().find(|p| p.id == "example").unwrap();
        let dir = download(&url, entry).unwrap();
        let p = Plugin::load(&dir).unwrap();
        assert_eq!(p.manifest.version, entry.version);
        assert!(p.object_path().exists());

        // A tampered file is rejected and nothing is left behind.
        let mut bad = entry.clone();
        bad.files[1].sha256 = "00".repeat(32);
        let err = download(&url, &bad).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!dir.exists());

        assert!(fetch_index("http://example.com/index.json").is_err());
        let _ = fs::remove_dir_all(&tmp);
    }
}
