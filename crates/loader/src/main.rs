//! kerntablet-loader — root-only, short-lived.
//!
//! Loads the HID-BPF programs, attaches them to the tablet's HID device and
//! pins the links plus the maps into a private bpffs. After that it exits: the
//! transform lives entirely in the kernel, and the unprivileged GUI talks to
//! the pinned `config` maps directly.
//!
//! Chain on the device: kt_head -> installed plugins (by `order`) -> kt_tail.
//! Installing or removing a plugin re-attaches the whole chain.

use std::error::Error;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{chown, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kerntablet_common::bpfmap::{self, PIN_CONFIG, PIN_LINK, PIN_LINK_HEAD, PIN_PIPE, PIN_PLUGINS, PIN_STATE};
use kerntablet_common::plugin::{self, Plugin, KT_PIPE_SIZE, KT_PLUGIN_MAGIC};
use kerntablet_common::kernel::{KtConfig, KT_F_ENABLED};
use kerntablet_common::tablet::{self, HidDevice, TabletSpec, HID_SYSFS};
use kerntablet_common::{pin_dir, pin_name, state_file, PIN_ROOT, STATE_DIR};
use libbpf_rs::{MapCore, MapFlags, MapType, Object, ObjectBuilder, OpenObject, ProgramType};

type Res<T> = Result<T, Box<dyn Error>>;

static BPF_OBJ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kerntablet.bpf.o"));

const BPF_FS_MAGIC: i64 = 0xcafe4a11;
const HEAD_OPS: &str = "kt_head";
const TAIL_OPS: &str = "kt_tail";
const CONFIG_MAP: &str = "kt_config";
const STATE_MAP: &str = "kt_state";
const PIPE_MAP: &str = "kt_pipe";

struct Opts {
    group: String,
    force: bool,
}

fn usage() -> ExitCode {
    eprintln!(
        "kerntablet-loader — attaches the in-kernel tablet transform (needs root)\n\n\
         usage:\n  \
         kerntablet-loader attach <hid-device> [--group NAME] [--force]\n  \
         kerntablet-loader attach-all [--group NAME] [--force]\n  \
         kerntablet-loader detach <hid-device>\n  \
         kerntablet-loader detach-all\n  \
         kerntablet-loader status\n  \
         kerntablet-loader plugin-install <dir>   (install a downloaded plugin, re-attach)\n  \
         kerntablet-loader plugin-remove <id>     (uninstall a plugin, re-attach)\n  \
         kerntablet-loader check [<plugin-dir>]   (parse the BPF objects, no root needed)\n\n\
         <hid-device> is a name from {HID_SYSFS}, e.g. 0003:056A:037A.0001\n\
         --group: group allowed to change the area without root (default: input)"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = Opts { group: "input".into(), force: false };
    if let Some(i) = args.iter().position(|a| a == "--force") {
        args.remove(i);
        opts.force = true;
    }
    if let Some(i) = args.iter().position(|a| a == "--group") {
        if i + 1 >= args.len() {
            return usage();
        }
        opts.group = args.remove(i + 1);
        args.remove(i);
    }

    let res = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["attach", name] => attach(name, &opts),
        ["attach-all"] => attach_all(&opts),
        ["detach", name] => detach(name),
        ["detach-all"] => detach_all(),
        ["status"] => status(),
        ["check"] => check(),
        ["check", dir] => check_plugin(Path::new(dir)).map(|p| println!("plugin {} OK", p.id())),
        ["plugin-install", dir] => plugin_install(Path::new(dir), &opts),
        ["plugin-remove", id] => plugin_remove(id, &opts),
        _ => return usage(),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kerntablet-loader: {e}");
            ExitCode::FAILURE
        }
    }
}

fn attach_all(opts: &Opts) -> Res<()> {
    let devs: Vec<_> = tablet::list_hid_devices()
        .into_iter()
        .filter(|d| tablet::match_spec(d).is_some())
        .collect();
    if devs.is_empty() {
        println!("no supported tablet connected");
    }
    for d in devs {
        attach(&d.name, opts)?;
    }
    Ok(())
}

fn attach(name: &str, opts: &Opts) -> Res<()> {
    let Some(dev) = tablet::read_hid_device(name) else {
        return Err(format!("{name}: no such HID device in {HID_SYSFS}").into());
    };
    let Some(spec) = tablet::match_spec(&dev) else {
        // Not ours (other device or the tablet's second interface): nothing to do.
        println!("{name}: not a supported pen interface, skipping");
        return Ok(());
    };

    ensure_bpffs()?;
    cleanup_stale();

    let dir = pin_dir(&dev.name);
    if dir.join(PIN_LINK).exists() {
        if !opts.force {
            println!("{name}: already attached");
            return Ok(());
        }
        remove_pins(&dir);
    } else if dir.exists() {
        remove_pins(&dir); // leftovers of a failed attach
    }

    if dev.driver.as_deref() != Some(spec.driver) {
        eprintln!(
            "warning: {name} is bound to '{}' instead of '{}' — the transform is attached, \
             but there will be no pen device until the '{}' module handles the tablet",
            dev.driver.as_deref().unwrap_or("no driver"),
            spec.driver,
            spec.driver
        );
    }

    let gid = lookup_gid(&opts.group);
    if gid.is_none() {
        eprintln!("warning: group '{}' not found, only root will be able to change the area", opts.group);
    }

    if let Err(e) = load_and_pin(&dev, spec, &dir, gid) {
        remove_pins(&dir);
        return Err(e);
    }
    prepare_state_dir(gid);
    println!("{name}: {} attached (hid id {})", spec.name, dev.hid_id);
    Ok(())
}

/// Copies an ELF image into an aligned buffer (libelf wants that).
fn aligned(bytes: &[u8]) -> Vec<u64> {
    let mut image = vec![0u64; bytes.len().div_ceil(8)];
    // SAFETY: plain byte copy into a buffer that is at least as large.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), image.as_mut_ptr().cast::<u8>(), bytes.len()) };
    image
}

fn open_image(name: &str, bytes: &[u8], hid_id: u32) -> Res<OpenObject> {
    let image = aligned(bytes);
    // SAFETY: the buffer holds `bytes.len()` initialised bytes.
    let image_bytes = unsafe { std::slice::from_raw_parts(image.as_ptr().cast::<u8>(), bytes.len()) };
    let mut builder = ObjectBuilder::default();
    builder.name(name)?;
    let mut open = builder.open_memory(image_bytes)?;
    set_hid_id(&mut open, hid_id)?;
    Ok(open)
}

/// Points every struct_ops in the object at `hid_id` (hid_bpf_ops.hid_id is
/// the first member of the struct_ops value). Returns their names.
fn set_hid_id(open: &mut OpenObject, hid_id: u32) -> Res<Vec<String>> {
    let mut names = Vec::new();
    for mut map in open.maps_mut() {
        if map.map_type() == MapType::StructOps {
            let name = map.name().to_string_lossy().into_owned();
            let data = map
                .initial_value_mut()
                .filter(|d| d.len() >= 4)
                .ok_or_else(|| format!("struct_ops {name} has no initial value"))?;
            data[..4].copy_from_slice(&(hid_id as i32).to_ne_bytes());
            names.push(name);
        }
    }
    Ok(names)
}

/// Opens the embedded core object.
fn open_object(hid_id: u32) -> Res<OpenObject> {
    let open = open_image("kerntablet", BPF_OBJ, hid_id)?;
    for want in [HEAD_OPS, TAIL_OPS] {
        if !open.maps().any(|m| m.name() == want) {
            return Err(format!("struct_ops {want} missing from the BPF object").into());
        }
    }
    Ok(open)
}

/// Opens a plugin's object and checks that it is what a plugin may be: one
/// hid_bpf_ops struct_ops, only struct_ops programs, plain data maps, a
/// config map of the size the manifest describes and the shared pipe map.
fn open_plugin(p: &Plugin, hid_id: u32) -> Res<OpenObject> {
    let bytes = fs::read(p.object_path()).map_err(|e| format!("{}: {e}", p.object_path().display()))?;
    let mut open = open_image(p.id(), &bytes, hid_id)?;
    let ops = set_hid_id(&mut open, hid_id)?;
    if ops.len() != 1 {
        return Err(format!("{}: expected exactly one struct_ops, found {}", p.id(), ops.len()).into());
    }
    for prog in open.progs() {
        if prog.prog_type() != ProgramType::StructOps {
            return Err(format!(
                "{}: program {} is {:?}; plugins may only contain HID-BPF struct_ops programs",
                p.id(),
                prog.name().to_string_lossy(),
                prog.prog_type()
            )
            .into());
        }
    }
    let (mut cfg_ok, mut pipe_ok) = (false, false);
    for map in open.maps() {
        let name = map.name().to_string_lossy().into_owned();
        match map.map_type() {
            MapType::Array | MapType::PercpuArray | MapType::Hash | MapType::PercpuHash | MapType::LruHash
            | MapType::StructOps => {}
            t => return Err(format!("{}: map {name} has unsupported type {t:?}", p.id()).into()),
        }
        if name == plugin::CONFIG_MAP {
            if map.value_size() as usize != p.config_size() {
                return Err(format!(
                    "{}: config map is {} bytes, plugin.toml describes {}",
                    p.id(),
                    map.value_size(),
                    p.config_size()
                )
                .into());
            }
            cfg_ok = true;
        }
        if name == plugin::PIPE_MAP {
            if map.value_size() != KT_PIPE_SIZE {
                return Err(format!("{}: kt_pipe is {} bytes, expected {KT_PIPE_SIZE}", p.id(), map.value_size()).into());
            }
            pipe_ok = true;
        }
    }
    if !cfg_ok || !pipe_ok {
        return Err(format!("{}: needs the {} and {} maps", p.id(), plugin::CONFIG_MAP, plugin::PIPE_MAP).into());
    }
    Ok(open)
}

fn check_plugin(dir: &Path) -> Res<Plugin> {
    let p = Plugin::load(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    open_plugin(&p, 1)?;
    Ok(p)
}

/// Sanity check of the embedded object that works without privileges.
fn check() -> Res<()> {
    let open = open_object(1)?;
    for map in open.maps() {
        println!(
            "map  {:<12} {:?} value_size={}",
            map.name().to_string_lossy(),
            map.map_type(),
            map.value_size()
        );
    }
    for prog in open.progs() {
        println!("prog {:<12} {:?}", prog.name().to_string_lossy(), prog.prog_type());
    }
    println!("BPF object OK ({} bytes)", BPF_OBJ.len());
    Ok(())
}

fn load_and_pin(dev: &HidDevice, spec: &TabletSpec, dir: &Path, gid: Option<u32>) -> Res<()> {
    let open = open_object(dev.hid_id)?;
    let mut obj = open.load().map_err(|e| format!("loading the BPF program: {e}"))?;

    mkdir(dir)?;

    let initial = saved_config(spec).unwrap_or_else(|| KtConfig::passthrough(spec.max_x, spec.max_y));
    let key = 0u32.to_ne_bytes();

    for mut map in obj.maps_mut() {
        let name = map.name().to_string_lossy().into_owned();
        match name.as_str() {
            CONFIG_MAP => {
                map.update(&key, initial.as_bytes(), MapFlags::ANY)
                    .map_err(|e| format!("writing the initial config: {e}"))?;
                let path = dir.join(PIN_CONFIG);
                map.pin(&path).map_err(|e| format!("pinning {}: {e}", path.display()))?;
                share(&path, gid, 0o660)?;
            }
            STATE_MAP => {
                let path = dir.join(PIN_STATE);
                map.pin(&path).map_err(|e| format!("pinning {}: {e}", path.display()))?;
                share(&path, gid, 0o664)?;
            }
            PIPE_MAP => {
                let path = dir.join(PIN_PIPE);
                map.pin(&path).map_err(|e| format!("pinning {}: {e}", path.display()))?;
            }
            _ => {}
        }
    }

    // Order of attachment = order of execution. Nothing touches the report
    // before kt_tail is attached, so a half-built chain is harmless.
    attach_ops(&mut obj, HEAD_OPS, &dir.join(PIN_LINK_HEAD), dev.hid_id)?;

    let (plugins, broken) = plugin::installed();
    for e in broken {
        eprintln!("warning: plugin {e} — skipped");
    }
    for p in &plugins {
        match attach_plugin(p, dev, spec, dir, gid) {
            Ok(restored) => {
                println!("{}: plugin {} {}{}", dev.name, p.id(), p.manifest.version, if restored { " (settings restored)" } else { "" })
            }
            Err(e) => {
                let _ = fs::remove_dir_all(dir.join(PIN_PLUGINS).join(p.id()));
                eprintln!("warning: {}: plugin {} not attached: {e}", dev.name, p.id());
            }
        }
    }

    attach_ops(&mut obj, TAIL_OPS, &dir.join(PIN_LINK), dev.hid_id)?;

    if initial.flags & KT_F_ENABLED != 0 {
        println!("{}: restored last applied area", dev.name);
    }
    Ok(())
}

fn mkdir(dir: &Path) -> Res<()> {
    fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(|e| format!("chmod {}: {e}", dir.display()))?;
    Ok(())
}

fn attach_ops(obj: &mut Object, name: &str, pin: &Path, hid_id: u32) -> Res<()> {
    let mut map = obj
        .maps_mut()
        .find(|m| m.name() == name)
        .ok_or_else(|| format!("struct_ops {name} missing"))?;
    let mut link = map.attach_struct_ops().map_err(|e| format!("attaching {name} to HID device {hid_id}: {e}"))?;
    link.pin(pin).map_err(|e| format!("pinning {}: {e}", pin.display()))?;
    Ok(())
}

/// Loads one plugin into the chain. Returns whether saved settings were restored.
fn attach_plugin(p: &Plugin, dev: &HidDevice, spec: &TabletSpec, dir: &Path, gid: Option<u32>) -> Res<bool> {
    let mut open = open_plugin(p, dev.hid_id)?;
    let pipe = dir.join(PIN_PIPE);
    for mut map in open.maps_mut() {
        if map.name() == plugin::PIPE_MAP {
            map.reuse_pinned_map(&pipe).map_err(|e| format!("sharing kt_pipe: {e}"))?;
        }
    }
    let mut obj = open.load().map_err(|e| format!("loading: {e}"))?;

    let pdir = dir.join(PIN_PLUGINS).join(p.id());
    mkdir(&dir.join(PIN_PLUGINS))?;
    mkdir(&pdir)?;

    let saved = fs::read(plugin::state_file(spec.vid, spec.pid, p.id()))
        .ok()
        .filter(|b| b.len() == p.config_size() && b[..4] == KT_PLUGIN_MAGIC.to_le_bytes());
    let key = 0u32.to_ne_bytes();
    let mut ops = None;
    for mut map in obj.maps_mut() {
        let name = map.name().to_string_lossy().into_owned();
        if name == plugin::CONFIG_MAP {
            if let Some(b) = &saved {
                map.update(&key, b, MapFlags::ANY).map_err(|e| format!("writing the saved config: {e}"))?;
            }
            let path = pdir.join(PIN_CONFIG);
            map.pin(&path).map_err(|e| format!("pinning {}: {e}", path.display()))?;
            share(&path, gid, 0o660)?;
        } else if map.map_type() == MapType::StructOps {
            ops = Some(name);
        }
    }
    let ops = ops.ok_or("no struct_ops")?;
    attach_ops(&mut obj, &ops, &pdir.join(PIN_LINK), dev.hid_id)?;
    Ok(saved.is_some())
}

/// Last config the user applied for this tablet model (written by the GUI).
fn saved_config(spec: &TabletSpec) -> Option<KtConfig> {
    let bytes = fs::read(state_file(spec.vid, spec.pid)).ok()?;
    let cfg = KtConfig::from_bytes(&bytes)?;
    let sane = cfg.max_x == spec.max_x
        && cfg.max_y == spec.max_y
        && cfg.clip_min_x <= cfg.clip_max_x
        && cfg.clip_min_y <= cfg.clip_max_y;
    sane.then_some(cfg)
}

fn share(path: &Path, gid: Option<u32>, mode: u32) -> Res<()> {
    if let Some(gid) = gid {
        chown(path, Some(0), Some(gid)).map_err(|e| format!("chown {}: {e}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

fn prepare_state_dir(gid: Option<u32>) {
    let dir = Path::new(STATE_DIR);
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Some(gid) = gid {
        let _ = chown(dir, Some(0), Some(gid));
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o2775));
    }
}

fn lookup_gid(group: &str) -> Option<u32> {
    let c = CString::new(group).ok()?;
    // SAFETY: getgrnam returns NULL or a pointer to static storage.
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    (!gr.is_null()).then(|| unsafe { (*gr).gr_gid })
}

/// Makes sure our own bpffs instance is mounted. A private instance is used
/// because /sys/fs/bpf is not traversable for normal users.
fn ensure_bpffs() -> Res<()> {
    let root = Path::new(PIN_ROOT);
    fs::create_dir_all(root).map_err(|e| format!("mkdir {PIN_ROOT}: {e}"))?;
    if let Some(parent) = root.parent() {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    }
    let c_root = CString::new(root.as_os_str().as_bytes())?;

    // SAFETY: statfs/mount with valid NUL-terminated strings.
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_root.as_ptr(), &mut st) != 0 {
            return Err(format!("statfs {PIN_ROOT}: {}", std::io::Error::last_os_error()).into());
        }
        if st.f_type as i64 == BPF_FS_MAGIC {
            return Ok(());
        }
        let r = libc::mount(
            c"bpf".as_ptr(),
            c_root.as_ptr(),
            c"bpf".as_ptr(),
            0,
            c"mode=0755".as_ptr().cast(),
        );
        if r != 0 {
            return Err(format!("mount bpffs on {PIN_ROOT}: {}", std::io::Error::last_os_error()).into());
        }
    }
    Ok(())
}

fn remove_pins(dir: &Path) {
    // Unpin the tail first so the report is never left half-processed.
    let _ = fs::remove_file(dir.join(PIN_LINK));
    let _ = fs::remove_dir_all(dir);
}

/// HID ids change on every replug; drop pins of devices that are gone.
fn cleanup_stale() {
    let Ok(rd) = fs::read_dir(PIN_ROOT) else { return };
    let present: Vec<String> = tablet::list_hid_devices().iter().map(|d| pin_name(&d.name)).collect();
    for e in rd.flatten() {
        if !present.contains(&e.file_name().to_string_lossy().into_owned()) {
            remove_pins(&e.path());
        }
    }
}

fn detach(name: &str) -> Res<()> {
    let dir = pin_dir(name);
    if !dir.exists() {
        println!("{name}: not attached");
        return Ok(());
    }
    remove_pins(&dir);
    if dir.exists() {
        return Err(format!("{name}: could not remove pins in {}", dir.display()).into());
    }
    println!("{name}: detached");
    Ok(())
}

fn detach_all() -> Res<()> {
    let Ok(rd) = fs::read_dir(PIN_ROOT) else {
        println!("nothing attached");
        return Ok(());
    };
    for e in rd.flatten() {
        detach(&e.file_name().to_string_lossy())?;
    }
    Ok(())
}

fn status() -> Res<()> {
    let mut any = false;
    for dev in tablet::list_hid_devices() {
        let Some(spec) = tablet::match_spec(&dev) else { continue };
        any = true;
        println!("{}  {}", dev.name, spec.name);
        println!("  kernel driver: {}", dev.driver.as_deref().unwrap_or("none"));
        if !bpfmap::is_attached(&dev.name) {
            println!("  transform:     not attached");
            continue;
        }
        match bpfmap::read_config(&dev.name) {
            Ok(cfg) if cfg.flags & KT_F_ENABLED != 0 => println!(
                "  transform:     attached, active (clip x {}..{}, y {}..{})",
                cfg.clip_min_x, cfg.clip_max_x, cfg.clip_min_y, cfg.clip_max_y
            ),
            Ok(_) => println!("  transform:     attached, passthrough (no area applied yet)"),
            Err(e) => println!("  transform:     attached, config unreadable: {e}"),
        }
        if let Ok(st) = bpfmap::StateReader::open(&dev.name).and_then(|r| r.read()) {
            println!("  pen reports:   {}", st.events);
        }
        for p in plugin::installed().0 {
            let state = if bpfmap::plugin_attached(&dev.name, p.id()) { "attached" } else { "NOT attached" };
            println!("  plugin:        {} {} — {state}", p.id(), p.manifest.version);
        }
    }
    if !any {
        println!("no supported tablet connected");
    }
    Ok(())
}

// ---------------------------------------------------------------- plugins

/// Copies a downloaded (or locally built) plugin into the root-owned plugin
/// directory and re-attaches every tablet.
fn plugin_install(src: &Path, opts: &Opts) -> Res<()> {
    let p = check_plugin(src)?;
    let root = plugin::plugin_dir();
    fs::create_dir_all(&root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
    let tmp = root.join(format!(".{}.tmp", p.id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir(&tmp)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    for name in [plugin::MANIFEST, p.manifest.object.as_str()] {
        let to = tmp.join(name);
        fs::copy(src.join(name), &to).map_err(|e| format!("copying {name}: {e}"))?;
        fs::set_permissions(&to, fs::Permissions::from_mode(0o644))?;
        let _ = chown(&to, Some(0), Some(0));
    }
    // Re-check what was actually copied.
    check_plugin(&tmp)?;
    let dest = root.join(p.id());
    let old = root.join(format!(".{}.old", p.id()));
    let _ = fs::remove_dir_all(&old);
    if dest.exists() {
        fs::rename(&dest, &old)?;
    }
    fs::rename(&tmp, &dest)?;
    let _ = fs::remove_dir_all(&old);
    println!("installed plugin {} {} into {}", p.id(), p.manifest.version, dest.display());
    reattach(opts)
}

fn plugin_remove(id: &str, opts: &Opts) -> Res<()> {
    if !plugin::valid_id(id) {
        return Err(format!("bad plugin id \"{id}\"").into());
    }
    let dir: PathBuf = plugin::plugin_dir().join(id);
    if !dir.exists() {
        return Err(format!("plugin {id} is not installed").into());
    }
    fs::remove_dir_all(&dir).map_err(|e| format!("removing {}: {e}", dir.display()))?;
    for spec in tablet::TABLETS {
        let _ = fs::remove_file(plugin::state_file(spec.vid, spec.pid, id));
    }
    println!("removed plugin {id}");
    reattach(opts)
}

/// Rebuilds the chain on every attached tablet.
fn reattach(opts: &Opts) -> Res<()> {
    let force = Opts { group: opts.group.clone(), force: true };
    attach_all(&force)
}
