//! ABI shared with the BPF program — mirrors `bpf/kerntablet.h`.

pub const KT_MAGIC: u32 = 0x3342_544b;
pub const KT_FIX_SHIFT: u32 = 20;

pub const KT_F_ENABLED: u32 = 1 << 0;
pub const KT_F_CLIP: u32 = 1 << 1;
pub const KT_F_LIMIT: u32 = 1 << 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KtConfig {
    pub magic: u32,
    pub flags: u32,
    pub m: [i64; 6],
    pub clip_min_x: i32,
    pub clip_min_y: i32,
    pub clip_max_x: i32,
    pub clip_max_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KtState {
    pub events: u64,
    pub raw_x: u32,
    pub raw_y: u32,
    pub in_prox: u32,
    pub _pad: u32,
}

pub const KT_CONFIG_SIZE: usize = std::mem::size_of::<KtConfig>();
pub const KT_STATE_SIZE: usize = std::mem::size_of::<KtState>();

const _: () = assert!(KT_CONFIG_SIZE == 80);
const _: () = assert!(KT_STATE_SIZE == 24);

impl KtConfig {
    /// Reports pass through untouched.
    pub fn passthrough(max_x: i32, max_y: i32) -> Self {
        Self {
            magic: KT_MAGIC,
            flags: 0,
            m: [1 << KT_FIX_SHIFT, 0, 0, 0, 1 << KT_FIX_SHIFT, 0],
            clip_min_x: 0,
            clip_min_y: 0,
            clip_max_x: max_x,
            clip_max_y: max_y,
            max_x,
            max_y,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: repr(C) POD without padding (asserted size above).
        unsafe { std::slice::from_raw_parts(self as *const _ as *const u8, KT_CONFIG_SIZE) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != KT_CONFIG_SIZE {
            return None;
        }
        // SAFETY: every bit pattern is a valid KtConfig.
        let cfg: Self = unsafe { std::ptr::read_unaligned(b.as_ptr() as *const Self) };
        (cfg.magic == KT_MAGIC).then_some(cfg)
    }

    /// Mirror of `kt_tail_event` for an unfiltered report.
    /// `None` means the report is turned into "pen out of range".
    pub fn apply(&self, x: u16, y: u16) -> Option<(u16, u16)> {
        self.apply_q20((x as i64) << KT_FIX_SHIFT, (y as i64) << KT_FIX_SHIFT)
    }

    /// Same for a position in Q20 tablet units, as plugins leave it in `kt_pipe`.
    pub fn apply_q20(&self, x: i64, y: i64) -> Option<(u16, u16)> {
        if self.magic != KT_MAGIC || self.flags & KT_F_ENABLED == 0 {
            return Some(((x >> KT_FIX_SHIFT) as u16, (y >> KT_FIX_SHIFT) as u16));
        }
        self.transform(x >> (KT_FIX_SHIFT - 4), y >> (KT_FIX_SHIFT - 4))
    }

    /// Affine transform + clipping of a Q4 position.
    fn transform(&self, xq: i64, yq: i64) -> Option<(u16, u16)> {
        let sh = KT_FIX_SHIFT + 4;
        let mut ox = (self.m[0] * xq + self.m[1] * yq + (self.m[2] << 4)) >> sh;
        let mut oy = (self.m[3] * xq + self.m[4] * yq + (self.m[5] << 4)) >> sh;
        let outside = ox < self.clip_min_x as i64
            || ox > self.clip_max_x as i64
            || oy < self.clip_min_y as i64
            || oy > self.clip_max_y as i64;
        if outside && self.flags & KT_F_LIMIT != 0 {
            return None;
        }
        if self.flags & KT_F_CLIP != 0 {
            ox = ox.clamp(self.clip_min_x as i64, self.clip_max_x as i64);
            oy = oy.clamp(self.clip_min_y as i64, self.clip_max_y as i64);
        }
        ox = ox.clamp(0, self.max_x as i64);
        oy = oy.clamp(0, self.max_y as i64);
        Some((ox as u16, oy as u16))
    }
}

impl KtState {
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != KT_STATE_SIZE {
            return None;
        }
        // SAFETY: every bit pattern is a valid KtState.
        Some(unsafe { std::ptr::read_unaligned(b.as_ptr() as *const Self) })
    }
}
