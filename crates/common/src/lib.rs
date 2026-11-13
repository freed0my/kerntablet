//! Shared pieces of kerntablet: kernel ABI, tablet database, user config,
//! transform math, monitor detection and raw access to the pinned BPF maps.

pub mod bind;
pub mod bpfmap;
pub mod config;
pub mod desktop;
pub mod expr;
pub mod kernel;
pub mod plugin;
pub mod tablet;
pub mod transform;

use std::path::PathBuf;

/// Private bpffs instance where the loader pins the link and the maps.
pub const PIN_ROOT: &str = "/run/kerntablet/bpf";
/// Last applied kernel config per tablet model, re-loaded on hotplug/boot.
pub const STATE_DIR: &str = "/var/lib/kerntablet";

/// Pin directory of a HID device. bpffs rejects names containing a dot, so
/// `0003:056A:037A.0001` is stored as `0003_056A_037A_0001`.
pub fn pin_dir(hid_name: &str) -> PathBuf {
    PathBuf::from(PIN_ROOT).join(pin_name(hid_name))
}

pub fn pin_name(hid_name: &str) -> String {
    hid_name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

pub fn state_file(vid: u16, pid: u16) -> PathBuf {
    PathBuf::from(STATE_DIR).join(format!("{vid:04X}-{pid:04X}.cfg"))
}
