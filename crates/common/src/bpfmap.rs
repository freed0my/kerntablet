//! Minimal raw access to BPF maps pinned in bpffs. Needs no privileges
//! beyond file permissions on the pin, so the GUI can retune the in-kernel
//! transform without root and without linking libbpf.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::kernel::{KtConfig, KtState, KT_CONFIG_SIZE, KT_STATE_SIZE};

const BPF_MAP_LOOKUP_ELEM: libc::c_long = 1;
const BPF_MAP_UPDATE_ELEM: libc::c_long = 2;
const BPF_OBJ_GET: libc::c_long = 7;
const BPF_OBJ_GET_INFO_BY_FD: libc::c_long = 15;

const BPF_F_RDONLY: u32 = 1 << 3;

#[repr(C)]
#[derive(Default)]
struct ObjGetAttr {
    pathname: u64,
    bpf_fd: u32,
    file_flags: u32,
    path_fd: i32,
    _pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct MapElemAttr {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

fn sys_bpf<T>(cmd: libc::c_long, attr: &T) -> io::Result<libc::c_long> {
    // SAFETY: attr is a properly initialised bpf_attr prefix of the given size.
    let r = unsafe {
        libc::syscall(libc::SYS_bpf, cmd, attr as *const T, std::mem::size_of::<T>() as libc::c_uint)
    };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

#[repr(C)]
#[derive(Default)]
struct InfoAttr {
    bpf_fd: u32,
    info_len: u32,
    info: u64,
}

/// Prefix of `struct bpf_map_info`.
#[repr(C)]
#[derive(Default)]
struct MapInfo {
    map_type: u32,
    id: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
}

/// A single-entry array map opened from its bpffs pin.
pub struct PinnedMap {
    fd: OwnedFd,
}

impl PinnedMap {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
        let attr = ObjGetAttr {
            pathname: c.as_ptr() as u64,
            file_flags: if read_only { BPF_F_RDONLY } else { 0 },
            ..Default::default()
        };
        let fd = sys_bpf(BPF_OBJ_GET, &attr)?;
        // SAFETY: the kernel just handed us this fd.
        Ok(Self { fd: unsafe { OwnedFd::from_raw_fd(fd as i32) } })
    }

    pub fn value_size(&self) -> io::Result<u32> {
        let mut info = MapInfo::default();
        let attr = InfoAttr {
            bpf_fd: self.fd.as_raw_fd() as u32,
            info_len: std::mem::size_of::<MapInfo>() as u32,
            info: &mut info as *mut MapInfo as u64,
        };
        sys_bpf(BPF_OBJ_GET_INFO_BY_FD, &attr)?;
        Ok(info.value_size)
    }

    /// Reads element 0. `value` must be exactly the map's value size.
    pub fn lookup(&self, value: &mut [u8]) -> io::Result<()> {
        let key: u32 = 0;
        let attr = MapElemAttr {
            map_fd: self.fd.as_raw_fd() as u32,
            key: &key as *const u32 as u64,
            value: value.as_mut_ptr() as u64,
            ..Default::default()
        };
        sys_bpf(BPF_MAP_LOOKUP_ELEM, &attr).map(|_| ())
    }

    /// Writes element 0. `value` must be exactly the map's value size.
    pub fn update(&self, value: &[u8]) -> io::Result<()> {
        let key: u32 = 0;
        let attr = MapElemAttr {
            map_fd: self.fd.as_raw_fd() as u32,
            key: &key as *const u32 as u64,
            value: value.as_ptr() as u64,
            ..Default::default()
        };
        sys_bpf(BPF_MAP_UPDATE_ELEM, &attr).map(|_| ())
    }
}

/// The last program of the chain (kt_tail); its presence means "attached".
pub const PIN_LINK: &str = "link";
pub const PIN_LINK_HEAD: &str = "link_head";
pub const PIN_CONFIG: &str = "config";
pub const PIN_STATE: &str = "state";
pub const PIN_PIPE: &str = "pipe";
/// `plugins/<id>/{link,config}` inside a device's pin directory.
pub const PIN_PLUGINS: &str = "plugins";

pub fn plugin_pin_dir(hid_name: &str, id: &str) -> std::path::PathBuf {
    crate::pin_dir(hid_name).join(PIN_PLUGINS).join(id)
}

/// Is this plugin attached to the device?
pub fn plugin_attached(hid_name: &str, id: &str) -> bool {
    plugin_pin_dir(hid_name, id).join(PIN_LINK).exists()
}

/// Writes a plugin's config. The size must match the map's value size, which
/// is checked against the manifest when the loader attaches the plugin.
pub fn write_plugin_config(hid_name: &str, id: &str, bytes: &[u8]) -> io::Result<()> {
    let map = PinnedMap::open(&plugin_pin_dir(hid_name, id).join(PIN_CONFIG), false)?;
    let size = map.value_size()?;
    if size as usize != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config size mismatch: kernel {size}, manifest {} (reinstall the plugin)", bytes.len()),
        ));
    }
    map.update(bytes)
}

/// Is the BPF program pinned (i.e. attached) for this HID device?
pub fn is_attached(hid_name: &str) -> bool {
    crate::pin_dir(hid_name).join(PIN_LINK).exists()
}

/// Push a transform into the kernel.
pub fn write_config(hid_name: &str, cfg: &KtConfig) -> io::Result<()> {
    PinnedMap::open(&crate::pin_dir(hid_name).join(PIN_CONFIG), false)?.update(cfg.as_bytes())
}

pub fn read_config(hid_name: &str) -> io::Result<KtConfig> {
    let map = PinnedMap::open(&crate::pin_dir(hid_name).join(PIN_CONFIG), true)?;
    let mut buf = [0u8; KT_CONFIG_SIZE];
    map.lookup(&mut buf)?;
    KtConfig::from_bytes(&buf)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad magic in kernel config"))
}

/// Live pen state published by the BPF program.
pub struct StateReader {
    map: PinnedMap,
}

impl StateReader {
    pub fn open(hid_name: &str) -> io::Result<Self> {
        Ok(Self { map: PinnedMap::open(&crate::pin_dir(hid_name).join(PIN_STATE), true)? })
    }

    pub fn read(&self) -> io::Result<KtState> {
        let mut buf = [0u8; KT_STATE_SIZE];
        self.map.lookup(&mut buf)?;
        Ok(KtState::from_bytes(&buf).unwrap_or_default())
    }
}
