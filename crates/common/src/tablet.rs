//! Supported tablets and their discovery through sysfs.

use std::fs;
use std::path::{Path, PathBuf};

/// Static description of a tablet model. All models listed here use the Wacom
/// "Bamboo pen" report layout that `bpf/kerntablet.bpf.c` rewrites (the one
/// OpenTabletDriver parses with `Wacom.Intuos.IntuosReportParser`; axis
/// ranges and sizes match OTD's configuration files).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabletSpec {
    pub name: &'static str,
    pub vid: u16,
    pub pid: u16,
    /// USB interface that carries the pen reports.
    pub interface: u8,
    /// Logical maximum of the axes as advertised by the kernel driver.
    pub max_x: i32,
    pub max_y: i32,
    /// Physical size of the active surface.
    pub width_mm: f64,
    pub height_mm: f64,
    /// Kernel driver that must own the HID device.
    pub driver: &'static str,
}

pub const TABLETS: &[TabletSpec] = &[
    TabletSpec {
        name: "Wacom CTL-472 (One by Wacom S)",
        vid: 0x056a,
        pid: 0x037a,
        interface: 0,
        max_x: 15200,
        max_y: 9500,
        width_mm: 152.0,
        height_mm: 95.0,
        driver: "wacom",
    },
    TabletSpec {
        name: "Wacom CTL-672 (One by Wacom M)",
        vid: 0x056a,
        pid: 0x037b,
        interface: 0,
        max_x: 21600,
        max_y: 13500,
        width_mm: 216.0,
        height_mm: 135.0,
        driver: "wacom",
    },
    TabletSpec {
        name: "Wacom CTL-480 (Intuos Pen S)",
        vid: 0x056a,
        pid: 0x030e,
        interface: 0,
        max_x: 15200,
        max_y: 9500,
        width_mm: 152.0,
        height_mm: 95.0,
        driver: "wacom",
    },
    TabletSpec {
        name: "Wacom CTL-470 (Bamboo Connect)",
        vid: 0x056a,
        pid: 0x00dd,
        interface: 0,
        max_x: 14720,
        max_y: 9200,
        width_mm: 147.2,
        height_mm: 92.0,
        driver: "wacom",
    },
];

pub fn find_spec(vid: u16, pid: u16) -> Option<&'static TabletSpec> {
    TABLETS.iter().find(|t| t.vid == vid && t.pid == pid)
}

/// A HID device currently present in sysfs.
#[derive(Debug, Clone)]
pub struct HidDevice {
    /// sysfs name, e.g. `0003:056A:037A.0001`
    pub name: String,
    pub sysfs: PathBuf,
    pub bus: u16,
    pub vid: u16,
    pub pid: u16,
    /// Kernel-wide HID id (the hex suffix of the name) — what `hid_bpf_ops.hid_id` wants.
    pub hid_id: u32,
    pub interface: Option<u8>,
    pub driver: Option<String>,
}

pub const HID_SYSFS: &str = "/sys/bus/hid/devices";

/// Parses `0003:056A:037A.0001`.
pub fn parse_hid_name(name: &str) -> Option<(u16, u16, u16, u32)> {
    let (ids, suffix) = name.split_once('.')?;
    let mut it = ids.split(':');
    let bus = u16::from_str_radix(it.next()?, 16).ok()?;
    let vid = u16::from_str_radix(it.next()?, 16).ok()?;
    let pid = u16::from_str_radix(it.next()?, 16).ok()?;
    if it.next().is_some() {
        return None;
    }
    let hid_id = u32::from_str_radix(suffix, 16).ok()?;
    Some((bus, vid, pid, hid_id))
}

pub fn read_hid_device(name: &str) -> Option<HidDevice> {
    let (bus, vid, pid, hid_id) = parse_hid_name(name)?;
    let link = Path::new(HID_SYSFS).join(name);
    let sysfs = fs::canonicalize(&link).ok()?;
    let interface = sysfs
        .parent()
        .and_then(|p| fs::read_to_string(p.join("bInterfaceNumber")).ok())
        .and_then(|s| u8::from_str_radix(s.trim(), 16).ok());
    let driver = fs::read_link(sysfs.join("driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    Some(HidDevice { name: name.to_string(), sysfs, bus, vid, pid, hid_id, interface, driver })
}

pub fn list_hid_devices() -> Vec<HidDevice> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(HID_SYSFS) {
        for e in rd.flatten() {
            if let Some(d) = read_hid_device(&e.file_name().to_string_lossy()) {
                out.push(d);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Does this HID device carry the pen reports of a supported tablet?
pub fn match_spec(dev: &HidDevice) -> Option<&'static TabletSpec> {
    let spec = find_spec(dev.vid, dev.pid)?;
    // If the interface number is unknown (non-USB transport), accept it.
    match dev.interface {
        Some(i) if i != spec.interface => None,
        _ => Some(spec),
    }
}

/// Every supported tablet that is plugged in, sorted by HID name.
pub fn find_tablets() -> Vec<(HidDevice, &'static TabletSpec)> {
    list_hid_devices().into_iter().filter_map(|d| match_spec(&d).map(|s| (d, s))).collect()
}

/// An input (evdev) device the kernel driver created for a HID device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputNode {
    /// e.g. "Wacom One by Wacom S Pen"
    pub name: String,
    /// e.g. "event2"
    pub event: Option<String>,
}

/// What compositors need to recognise our tablet among their input devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabletInputs {
    pub vid: u16,
    pub pid: u16,
    pub nodes: Vec<InputNode>,
}

impl TabletInputs {
    pub fn of(dev: &HidDevice) -> Self {
        Self { vid: dev.vid, pid: dev.pid, nodes: input_nodes(&dev.sysfs) }
    }

    /// Does an input device name reported by a compositor belong to us?
    /// Compared on letters and digits only: Hyprland turns
    /// "Wacom One by Wacom S Pen" into "wacom-one-by-wacom-s-pen", sway into
    /// "Wacom_One_by_Wacom_S_Pen", the X wacom driver appends " stylus".
    pub fn matches_name(&self, name: &str) -> bool {
        let key = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
        let name = key(name);
        self.nodes.iter().any(|n| {
            let ours = key(&n.name);
            !ours.is_empty() && name.starts_with(&ours)
        })
    }
}

/// `<hid sysfs>/input/input*/{name,event*}`
pub fn input_nodes(hid_sysfs: &Path) -> Vec<InputNode> {
    let mut nodes = Vec::new();
    let Ok(dir) = fs::read_dir(hid_sysfs.join("input")) else { return nodes };
    for e in dir.flatten() {
        let p = e.path();
        let Ok(name) = fs::read_to_string(p.join("name")) else { continue };
        let event = fs::read_dir(&p).ok().and_then(|d| {
            d.flatten().map(|x| x.file_name().to_string_lossy().into_owned()).find(|n| n.starts_with("event"))
        });
        nodes.push(InputNode { name: name.trim().to_string(), event });
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    nodes
}

/// A tablet model, written as `056a:037a` in config.toml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabletId {
    pub vid: u16,
    pub pid: u16,
}

impl TabletId {
    pub fn of(spec: &TabletSpec) -> Self {
        Self { vid: spec.vid, pid: spec.pid }
    }
}

impl std::fmt::Display for TabletId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vid, self.pid)
    }
}

impl std::str::FromStr for TabletId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (v, p) = s.split_once(':').ok_or_else(|| format!("expected vid:pid, got {s:?}"))?;
        let hex = |x: &str| u16::from_str_radix(x.trim(), 16).map_err(|e| format!("{s:?}: {e}"));
        Ok(Self { vid: hex(v)?, pid: hex(p)? })
    }
}

impl serde::Serialize for TabletId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for TabletId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hid_names() {
        assert_eq!(parse_hid_name("0003:056A:037A.0001"), Some((3, 0x056a, 0x037a, 1)));
        assert_eq!(parse_hid_name("0003:056A:037A.00AF"), Some((3, 0x056a, 0x037a, 0xaf)));
        assert_eq!(parse_hid_name("garbage"), None);
        assert_eq!(parse_hid_name("0003:056A.0001"), None);
    }

    #[test]
    fn tablet_id_roundtrip() {
        let id: TabletId = "056a:037A".parse().unwrap();
        assert_eq!(id, TabletId { vid: 0x056a, pid: 0x037a });
        assert_eq!(id.to_string(), "056a:037a");
        assert!("nope".parse::<TabletId>().is_err());
    }

    #[test]
    fn models_are_unique() {
        for (i, a) in TABLETS.iter().enumerate() {
            assert!(TABLETS[i + 1..].iter().all(|b| (a.vid, a.pid) != (b.vid, b.pid)), "{}", a.name);
            assert!((a.max_x as f64 / 100.0 - a.width_mm).abs() < 0.01, "{}", a.name);
        }
    }
}
