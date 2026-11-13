//! Turns the user's areas into the fixed-point matrix the BPF program applies.
//!
//! Pipeline (identical to OpenTabletDriver's absolute mode):
//!   raw -> mm -> translate to area centre -> rotate by -rotation ->
//!   scale mm to px -> translate to display-area centre (desktop px)
//! and then one extra step that OTD does not need: desktop px are converted
//! back into *tablet units*, because the kernel driver still advertises the
//! tablet's native axis range and the compositor / X server stretches that
//! range over the whole desktop.

use crate::config::{Config, TabletArea};
use crate::desktop::Target;
use crate::kernel::{KtConfig, KT_FIX_SHIFT, KT_F_CLIP, KT_F_ENABLED, KT_F_LIMIT, KT_MAGIC};
use crate::tablet::TabletSpec;

/// `desk_w` x `desk_h` is the size of the region the compositor maps the
/// tablet's full axis range onto (the bounding box of all monitors).
pub fn compute(spec: &TabletSpec, cfg: &Config, desk_w: f64, desk_h: f64) -> KtConfig {
    compute_for(spec, cfg, &Target { output: None, x: 0.0, y: 0.0, width: desk_w, height: desk_h })
}

/// Same, for a tablet the compositor has bound to `target` (a monitor or the
/// whole desktop); the display area stays in desktop coordinates.
pub fn compute_for(spec: &TabletSpec, cfg: &Config, target: &Target) -> KtConfig {
    let pass = KtConfig::passthrough(spec.max_x, spec.max_y);
    let (desk_w, desk_h) = (target.width, target.height);
    let t = &cfg.tablet;
    let mut d = cfg.display;
    d.x -= target.x;
    d.y -= target.y;
    let d = &d;
    let sane = [t.width, t.height, d.width, d.height, desk_w, desk_h]
        .iter()
        .all(|v| v.is_finite() && *v >= 1e-3);
    if !cfg.enabled || !sane {
        return pass;
    }

    // raw units -> mm
    let sx = spec.width_mm / spec.max_x as f64;
    let sy = spec.height_mm / spec.max_y as f64;
    let r = (-t.rotation).to_radians();
    let (sin, cos) = r.sin_cos();
    // mm -> px
    let px = d.width / t.width;
    let py = d.height / t.height;
    // desktop px -> output units. libinput normalises with (max - min + 1).
    let kx = (spec.max_x as f64 + 1.0) / desk_w;
    let ky = (spec.max_y as f64 + 1.0) / desk_h;

    let a = [
        sx * cos * px * kx,
        -sy * sin * px * kx,
        ((-t.x * cos + t.y * sin) * px + d.x) * kx,
        sx * sin * py * ky,
        sy * cos * py * ky,
        ((-t.x * sin - t.y * cos) * py + d.y) * ky,
    ];
    let one = (1u64 << KT_FIX_SHIFT) as f64;
    let mut m = [0i64; 6];
    for (dst, src) in m.iter_mut().zip(a) {
        let v = (src * one).round();
        // The kernel multiplies by a Q4 position (|pos| < 2^22 with prediction
        // overshoot): keep the products far away from i64 overflow.
        if !v.is_finite() || v.abs() > (1u64 << 36) as f64 {
            return pass;
        }
        *dst = v as i64;
    }

    // Display area in output units. The upper bound is exclusive in pixel
    // space so the cursor can never spill onto a neighbouring monitor.
    let lo = |edge: f64, k: f64, max: i32| ((edge * k).ceil() as i64).clamp(0, max as i64) as i32;
    let hi = |edge: f64, k: f64, max: i32| (((edge * k).ceil() as i64) - 1).clamp(0, max as i64) as i32;
    let clip_min_x = lo(d.x - d.width / 2.0, kx, spec.max_x);
    let clip_max_x = hi(d.x + d.width / 2.0, kx, spec.max_x).max(clip_min_x);
    let clip_min_y = lo(d.y - d.height / 2.0, ky, spec.max_y);
    let clip_max_y = hi(d.y + d.height / 2.0, ky, spec.max_y).max(clip_min_y);

    let mut flags = KT_F_ENABLED;
    if cfg.clip {
        flags |= KT_F_CLIP;
    }
    if cfg.limit {
        flags |= KT_F_LIMIT;
    }

    KtConfig {
        magic: KT_MAGIC,
        flags,
        m,
        clip_min_x,
        clip_min_y,
        clip_max_x,
        clip_max_y,
        ..pass
    }
}

/// Corners of the (rotated) tablet area in mm: TL, TR, BR, BL of the area as
/// it maps onto the display's TL, TR, BR, BL.
pub fn area_corners(t: &TabletArea) -> [(f64, f64); 4] {
    let (sin, cos) = t.rotation.to_radians().sin_cos();
    let (hw, hh) = (t.width / 2.0, t.height / 2.0);
    [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)]
        .map(|(qx, qy)| (t.x + qx * cos - qy * sin, t.y + qx * sin + qy * cos))
}

/// Axis-aligned bounding box (min_x, min_y, max_x, max_y) of the rotated area.
pub fn area_bounds(t: &TabletArea) -> (f64, f64, f64, f64) {
    let c = area_corners(t);
    let xs = c.map(|p| p.0);
    let ys = c.map(|p| p.1);
    (
        xs.iter().cloned().fold(f64::INFINITY, f64::min),
        ys.iter().cloned().fold(f64::INFINITY, f64::min),
        xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DisplayArea, TabletArea};
    use crate::tablet::TABLETS;

    const DESK: (f64, f64) = (3840.0, 1080.0);

    fn spec() -> &'static TabletSpec {
        &TABLETS[0]
    }

    /// Output units -> desktop pixel, the way libinput + compositor do it.
    fn to_px(spec: &TabletSpec, out: (u16, u16)) -> (f64, f64) {
        (
            out.0 as f64 * DESK.0 / (spec.max_x as f64 + 1.0),
            out.1 as f64 * DESK.1 / (spec.max_y as f64 + 1.0),
        )
    }

    fn raw(spec: &TabletSpec, mm: (f64, f64)) -> (u16, u16) {
        (
            (mm.0 / spec.width_mm * spec.max_x as f64).round() as u16,
            (mm.1 / spec.height_mm * spec.max_y as f64).round() as u16,
        )
    }

    fn otd_profile() -> Config {
        Config {
            enabled: true,
            lock_aspect: false,
            clip: true,
            limit: false,
            bind_output: crate::config::BindMode::Auto,
            preferred: None,
            display: DisplayArea { width: 1920.0, height: 1080.0, x: 2880.0, y: 540.0 },
            tablet: TabletArea { width: 58.0, height: 39.55, x: 123.0, y: 75.225, rotation: 180.0 },
        }
    }

    #[test]
    fn centre_maps_to_centre() {
        let k = compute(spec(), &otd_profile(), DESK.0, DESK.1);
        let (x, y) = to_px(spec(), k.apply(12300, 7523).unwrap());
        assert!((x - 2880.0).abs() < 1.5, "x = {x}");
        assert!((y - 540.0).abs() < 1.5, "y = {y}");
    }

    #[test]
    fn rotation_180_flips_both_axes() {
        let cfg = otd_profile();
        let k = compute(spec(), &cfg, DESK.0, DESK.1);
        // 10 mm left / 5 mm up of the area centre on the tablet ...
        let p = raw(spec(), (cfg.tablet.x - 10.0, cfg.tablet.y - 5.0));
        let (x, y) = to_px(spec(), k.apply(p.0, p.1).unwrap());
        // ... is right / down of the display centre.
        let ex = 2880.0 + 10.0 * 1920.0 / 58.0;
        let ey = 540.0 + 5.0 * 1080.0 / 39.55;
        assert!((x - ex).abs() < 1.5, "x = {x}, expected {ex}");
        assert!((y - ey).abs() < 1.5, "y = {y}, expected {ey}");
    }

    #[test]
    fn rotation_90_matches_otd_convention() {
        let mut cfg = otd_profile();
        cfg.tablet = TabletArea { width: 40.0, height: 40.0, x: 76.0, y: 47.5, rotation: 90.0 };
        cfg.display = DisplayArea { width: 1000.0, height: 1000.0, x: 960.0, y: 540.0 };
        let k = compute(spec(), &cfg, DESK.0, DESK.1);
        // OTD rotates by -rotation: +X on the tablet becomes -Y on screen.
        let p = raw(spec(), (76.0 + 10.0, 47.5));
        let (x, y) = to_px(spec(), k.apply(p.0, p.1).unwrap());
        assert!((x - 960.0).abs() < 1.5, "x = {x}");
        assert!((y - (540.0 - 250.0)).abs() < 1.5, "y = {y}");
    }

    #[test]
    fn corners_agree_with_matrix() {
        let cfg = otd_profile();
        let k = compute(spec(), &cfg, DESK.0, DESK.1);
        let c = area_corners(&cfg.tablet);
        // TL corner of the area lands on the display's top-left pixel.
        let p = raw(spec(), c[0]);
        let (x, y) = to_px(spec(), k.apply(p.0, p.1).unwrap());
        assert!((x - 1920.0).abs() < 2.0, "x = {x}");
        assert!(y.abs() < 2.0, "y = {y}");
    }

    #[test]
    fn clipping_stays_on_the_target_monitor() {
        let mut cfg = otd_profile();
        // Map onto the *left* monitor: its right edge touches the other one.
        cfg.display.x = 960.0;
        let k = compute(spec(), &cfg, DESK.0, DESK.1);
        for (rx, ry) in [(0u16, 0u16), (15200, 9500), (0, 9500), (15200, 0), (7000, 3000)] {
            let (x, y) = to_px(spec(), k.apply(rx, ry).unwrap());
            assert!((0.0..1920.0).contains(&x), "x = {x} for raw {rx},{ry}");
            assert!((0.0..1080.0).contains(&y), "y = {y} for raw {rx},{ry}");
        }
    }

    #[test]
    fn limiting_drops_the_pen_outside_the_area() {
        let mut cfg = otd_profile();
        cfg.limit = true;
        let k = compute(spec(), &cfg, DESK.0, DESK.1);
        assert_eq!(k.apply(0, 0), None);
        assert!(k.apply(12300, 7523).is_some());
    }

    #[test]
    fn without_clipping_the_cursor_may_leave_the_area() {
        let mut cfg = otd_profile();
        cfg.clip = false;
        let k = compute(spec(), &cfg, DESK.0, DESK.1);
        // Far right of the tablet = far left on screen with 180 degree rotation.
        let (x, _) = to_px(spec(), k.apply(15200, 7523).unwrap());
        assert!(x < 1920.0, "x = {x}");
    }

    #[test]
    fn full_tablet_on_full_desktop_is_identity() {
        let s = spec();
        let cfg = Config {
            enabled: true,
            lock_aspect: false,
            clip: true,
            limit: false,
            bind_output: crate::config::BindMode::Auto,
            preferred: None,
            display: DisplayArea { width: DESK.0, height: DESK.1, x: DESK.0 / 2.0, y: DESK.1 / 2.0 },
            tablet: TabletArea {
                width: s.width_mm,
                height: s.height_mm,
                x: s.width_mm / 2.0,
                y: s.height_mm / 2.0,
                rotation: 0.0,
            },
        };
        let k = compute(s, &cfg, DESK.0, DESK.1);
        for (rx, ry) in [(0u16, 0u16), (15200, 9500), (4321, 1234)] {
            let (ox, oy) = k.apply(rx, ry).unwrap();
            assert!((ox as i32 - rx as i32).abs() <= 1 && (oy as i32 - ry as i32).abs() <= 1);
        }
    }

    #[test]
    fn bound_to_a_monitor_uses_the_full_range_for_it() {
        let cfg = otd_profile(); // DP-1 = right half of a 3840x1080 desktop
        let t = Target { output: Some("DP-1".into()), x: 1920.0, y: 0.0, width: 1920.0, height: 1080.0 };
        let k = compute_for(spec(), &cfg, &t);
        let s = spec();
        // Area centre -> centre of the output range.
        let (ox, oy) = k.apply(12300, 7523).unwrap();
        assert!((ox as f64 - (s.max_x as f64 + 1.0) / 2.0).abs() < 10.0, "ox = {ox}");
        assert!((oy as f64 - (s.max_y as f64 + 1.0) / 2.0).abs() < 10.0, "oy = {oy}");
        // Area corners -> range corners (180 deg: TL of area = TL of screen).
        let c = area_corners(&cfg.tablet);
        let p = raw(s, c[0]);
        let (ox, oy) = k.apply(p.0, p.1).unwrap();
        assert!(ox < 20 && oy < 20, "{ox},{oy}");
        assert_eq!((k.clip_min_x, k.clip_max_x), (0, s.max_x));
        assert_eq!((k.clip_min_y, k.clip_max_y), (0, s.max_y));
    }

    #[test]
    fn disabled_or_degenerate_is_passthrough() {
        let mut cfg = otd_profile();
        cfg.enabled = false;
        assert_eq!(compute(spec(), &cfg, DESK.0, DESK.1).apply(100, 200), Some((100, 200)));
        let mut cfg = otd_profile();
        cfg.tablet.width = 0.0;
        assert_eq!(compute(spec(), &cfg, DESK.0, DESK.1).apply(100, 200), Some((100, 200)));
    }
}
