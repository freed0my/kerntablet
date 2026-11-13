//! Logic shared by the GUI and the command line: device health and "apply".

use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use kerntablet_common::bind::{self, bound_by_us};
use kerntablet_common::bpfmap;
use kerntablet_common::config::{self, BindMode, Config};
use kerntablet_common::desktop::{Backend, Desktop, Target};
use kerntablet_common::plugin::{self, Plugin, PluginEntry, PluginSettings, Value};
use kerntablet_common::state_file;
use kerntablet_common::tablet::{self, HidDevice, TabletId, TabletInputs, TabletSpec, TABLETS};
use kerntablet_common::transform;

/// What stands between the user's settings and the kernel right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Health {
    NoTablet,
    /// The HID device is owned by another kernel driver (name inside).
    WrongDriver(String),
    /// The BPF program is not loaded for the device.
    NotAttached,
    /// Attached, but the pinned map cannot be opened by this user.
    NoAccess(String),
    Active,
}

impl Health {
    pub fn is_active(&self) -> bool {
        matches!(self, Health::Active)
    }

    pub fn describe(&self, spec: &TabletSpec) -> String {
        match self {
            Health::NoTablet => "Tablet not connected".into(),
            Health::WrongDriver(d) => format!(
                "Tablet is bound to driver \"{d}\", expected \"{}\" (kernel module not loaded — run install.sh)",
                spec.driver
            ),
            Health::NotAttached => {
                "BPF program is not loaded (sudo systemctl start 'kerntablet@*' or replug the tablet)".into()
            }
            Health::NoAccess(e) => format!("No access to the in-kernel settings: {e} (the input group is required)"),
            Health::Active => "Active in kernel".into(),
        }
    }
}

pub struct Probe {
    pub device: Option<HidDevice>,
    pub spec: &'static TabletSpec,
    pub health: Health,
    /// All supported tablets that are plugged in (the chosen one included).
    pub connected: Vec<(HidDevice, &'static TabletSpec)>,
}

/// Picks the tablet to configure: the preferred model if it is plugged in,
/// otherwise the first supported one.
pub fn probe(preferred: Option<TabletId>) -> Probe {
    let connected = tablet::find_tablets();
    let chosen = preferred
        .and_then(|p| connected.iter().find(|(_, s)| TabletId::of(s) == p))
        .or_else(|| connected.first())
        .cloned();
    let Some((device, spec)) = chosen else {
        let spec = preferred.and_then(|p| tablet::find_spec(p.vid, p.pid)).unwrap_or(&TABLETS[0]);
        return Probe { device: None, spec, health: Health::NoTablet, connected };
    };
    let health = health_of(&device, spec);
    Probe { device: Some(device), spec, health, connected }
}

pub fn health_of(device: &HidDevice, spec: &TabletSpec) -> Health {
    if !bpfmap::is_attached(&device.name) {
        match device.driver.as_deref() {
            Some(d) if d != spec.driver => Health::WrongDriver(d.to_string()),
            _ => Health::NotAttached,
        }
    } else if let Err(e) = bpfmap::read_config(&device.name) {
        Health::NoAccess(e.to_string())
    } else if device.driver.as_deref() != Some(spec.driver) {
        Health::WrongDriver(device.driver.clone().unwrap_or_else(|| "none".into()))
    } else {
        Health::Active
    }
}

/// The `preferred` model from the saved settings, if any.
pub fn saved_preference() -> Option<TabletId> {
    Config::load().ok().flatten().and_then(|c| c.preferred)
}

/// Saved settings, or an OpenTabletDriver import, or defaults — in that order.
pub fn initial_config(spec: &TabletSpec, desktop: &Desktop) -> (Config, &'static str) {
    let (mut cfg, origin) = match Config::load() {
        Ok(Some(c)) => (c, "loaded saved settings"),
        Ok(None) => match config::import_otd() {
            Some(c) => (c, "area imported from OpenTabletDriver"),
            None => (Config::default_for(spec, desktop), "default settings"),
        },
        Err(_) => (Config::default_for(spec, desktop), "settings file is corrupt — using defaults"),
    };
    cfg.sanitize(spec);
    (cfg, origin)
}

/// Pushes the matrix into the kernel; with `persist` also saves the settings
/// (config.toml and the loader's replug state), like OTD's Save vs Apply.
/// `Ok` carries a note for the user, `Err` explains what did not happen.
pub fn apply(cfg: &Config, probe: &Probe, desktop: &Desktop, persist: bool) -> Result<String, String> {
    if persist {
        cfg.save().map_err(|e| format!("could not save {}: {e}", Config::path().display()))?;
    }
    let saved = if persist { "settings saved, but " } else { "" };

    let Some(device) = &probe.device else {
        return Err(format!("{saved}the tablet is not connected"));
    };
    let (target, bind_note) = bind_target(cfg, desktop, &TabletInputs::of(device));
    let kcfg = transform::compute_for(probe.spec, cfg, &target);

    // Remember it for the loader (hotplug / reboot). Best effort.
    let state = state_file(probe.spec.vid, probe.spec.pid);
    let state_note = match persist.then(|| fs::write(&state, kcfg.as_bytes())) {
        None | Some(Ok(())) => "",
        Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => "",
        Some(Err(_)) => " (could not store it for replug)",
    };

    bpfmap::write_config(&device.name, &kcfg).map_err(|e| match probe.health {
        Health::Active | Health::NoAccess(_) => format!("{saved}the kernel rejected the settings: {e}"),
        _ => format!("{saved}not applied: {}", probe.health.describe(probe.spec)),
    })?;

    let on = match (&target.output, cfg.bind_output) {
        (Some(o), BindMode::Manual) => format!(", area computed for {o} (bound by your compositor settings)"),
        (Some(o), _) => format!(", tablet bound to {o}"),
        (None, _) => String::new(),
    };
    Ok(format!(
        "Applied in kernel: {:.2}×{:.2} mm → {:.0}×{:.0} px{on}{}{bind_note}",
        cfg.tablet.width, cfg.tablet.height, cfg.display.width, cfg.display.height, state_note
    ))
}

/// Binds the tablet in the compositor (in `auto` mode) and returns what the
/// kernel matrix must target. If binding to a monitor fails, falls back to
/// the whole desktop so the matrix still matches what the compositor does.
pub fn bind_target(cfg: &Config, desktop: &Desktop, tablet: &TabletInputs) -> (Target, String) {
    let backend = desktop.backend;
    let want = desktop.target_for(cfg);
    let unbind = |note: &mut String| {
        if let Err(e) = bind::bind(backend, tablet, None) {
            if note.is_empty() {
                *note = format!(" (could not remove the monitor binding: {e})");
            }
        }
    };
    let mut note = String::new();
    match cfg.bind_output {
        // The user's own compositor setting: leave the compositor alone.
        BindMode::Manual => return (want, note),
        BindMode::Off => {
            if bound_by_us(backend) {
                unbind(&mut note);
            }
            return (want, note);
        }
        BindMode::Auto => {}
    }
    if !backend.can_bind() {
        let one_monitor = desktop.monitors.len() > 1 && desktop.monitor_containing(&cfg.display).is_some();
        if cfg.enabled && one_monitor {
            note = format!(
                " (kerntablet cannot bind the tablet to a monitor on {} by itself — bind it in the compositor's \
                 own settings and set \"Monitor binding\" to manual)",
                std::env::var("XDG_CURRENT_DESKTOP").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| backend.label().into())
            );
        }
        return (want, note);
    }
    if let Some(out) = want.output.as_deref() {
        match bind::bind(backend, tablet, Some(out)) {
            Ok(n) => return (want, n),
            Err(e) => note = format!(" (could not bind to the monitor: {e} — mapping the whole desktop instead)"),
        }
    }
    // Runtime-only bindings are always ours to reset; persistent ones
    // (KDE, GNOME) only if kerntablet set them.
    if bound_by_us(backend) || matches!(backend, Backend::Hyprland | Backend::Sway | Backend::X11) {
        unbind(&mut note);
    }
    (desktop.whole(), note)
}

// ---------------------------------------------------------------- plugins

pub const LOADER: &str = "/usr/local/bin/kerntablet-loader";

/// Saved plugin settings with every installed plugin filled in.
pub fn plugin_settings(plugins: &[Plugin]) -> PluginSettings {
    let mut s = PluginSettings::load().unwrap_or_default();
    if !PluginSettings::path().exists() {
        migrate_builtin_antichatter(&mut s);
    }
    s.normalize(plugins);
    s
}

/// Settings of the antichatter that briefly lived in the core
/// (`[antichatter]` in config.toml) — same keys as the plugin's params.
fn migrate_builtin_antichatter(s: &mut PluginSettings) {
    let Ok(text) = fs::read_to_string(Config::path()) else { return };
    let Ok(v) = text.parse::<toml::Table>() else { return };
    let Some(t) = v.get("antichatter").and_then(|a| a.as_table()) else { return };
    let mut e = PluginEntry::default();
    for (k, v) in t {
        match (k.as_str(), v) {
            ("enabled", toml::Value::Boolean(b)) => e.enabled = *b,
            (_, toml::Value::Boolean(b)) => {
                e.values.insert(k.clone(), Value::Bool(*b));
            }
            (_, toml::Value::Float(f)) => {
                e.values.insert(k.clone(), Value::Num(*f));
            }
            (_, toml::Value::Integer(i)) => {
                e.values.insert(k.clone(), Value::Num(*i as f64));
            }
            _ => {}
        }
    }
    s.plugin.insert("devocub-antichatter".into(), e);
}

/// Writes the plugin settings into the kernel; with `persist` also saves them.
pub fn apply_plugins(settings: &PluginSettings, plugins: &[Plugin], probe: &Probe, persist: bool) -> Result<String, String> {
    if persist {
        settings.save().map_err(|e| format!("could not save {}: {e}", PluginSettings::path().display()))?;
    }
    let Some(device) = &probe.device else { return Ok(String::new()) };
    let mut on = Vec::new();
    let mut problems = Vec::new();
    for p in plugins {
        let entry = settings.plugin.get(p.id()).cloned().unwrap_or_default();
        let bytes = match p.config_bytes(&entry, probe.spec) {
            Ok(b) => b,
            Err(e) => {
                problems.push(e);
                continue;
            }
        };
        if persist {
            let _ = fs::write(plugin::state_file(probe.spec.vid, probe.spec.pid, p.id()), &bytes);
        }
        if !bpfmap::plugin_attached(&device.name, p.id()) {
            if entry.enabled {
                problems.push(format!("{} is installed but not loaded — replug the tablet", p.manifest.name));
            }
            continue;
        }
        match bpfmap::write_plugin_config(&device.name, p.id(), &bytes) {
            Ok(()) if entry.enabled => on.push(p.manifest.name.clone()),
            Ok(()) => {}
            Err(e) => problems.push(format!("{}: {e}", p.manifest.name)),
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    Ok(if on.is_empty() { String::new() } else { format!(" + {}", on.join(", ")) })
}

/// Runs `kerntablet-loader <args>` as root: sudo in a terminal, pkexec
/// (graphical password prompt) otherwise.
pub fn run_privileged(args: &[&str], terminal: bool) -> Result<String, String> {
    if !Path::new(LOADER).exists() {
        return Err(format!("{LOADER} not found — run install.sh first"));
    }
    if terminal {
        let st = Command::new("sudo").arg(LOADER).args(args).status().map_err(|e| format!("sudo: {e}"))?;
        return if st.success() { Ok(String::new()) } else { Err(format!("kerntablet-loader failed ({st})")) };
    }
    let out = Command::new("pkexec").arg(LOADER).args(args).output().map_err(|e| format!("pkexec: {e}"))?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    match out.status.code() {
        Some(0) => Ok(text(&out.stdout)),
        Some(126) | Some(127) => Err("not authorised (password prompt cancelled, or no polkit agent is running)".into()),
        _ => Err(text(&out.stderr).lines().last().unwrap_or("kerntablet-loader failed").to_string()),
    }
}

/// Installs a plugin by id from the repository, or from a local directory.
pub fn install_plugin(what: &str, repository: &str, terminal: bool) -> Result<String, String> {
    let local = Path::new(what);
    let dir = if what.contains('/') || local.is_dir() {
        local.canonicalize().map_err(|e| format!("{what}: {e}"))?
    } else {
        let idx = plugin::fetch_index(repository)?;
        let entry = idx
            .plugins
            .iter()
            .find(|p| p.id == what)
            .ok_or_else(|| format!("no plugin \"{what}\" in {repository}"))?;
        plugin::download(repository, entry)?
    };
    let dir = dir.to_str().ok_or("non-UTF-8 path")?.to_string();
    run_privileged(&["plugin-install", &dir], terminal)
}

pub fn remove_plugin(id: &str, terminal: bool) -> Result<String, String> {
    run_privileged(&["plugin-remove", id], terminal)
}
