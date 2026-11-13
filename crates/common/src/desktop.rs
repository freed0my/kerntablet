//! Monitor layout of the running session.
//!
//! Monitors come from the standard Wayland protocols (`wl_output` +
//! `xdg-output`, logical coordinates), so any Wayland compositor works; on
//! Xorg from `xrandr`. Unbound, compositors and the X server stretch a
//! tablet's full axis range over the bounding box of the whole desktop.
//! Monitor coordinates are reported relative to the box's top-left corner
//! (the same convention OpenTabletDriver uses).

use std::process::Command;

use crate::config::{BindMode, Config, DisplayArea};

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The session, as far as binding the tablet to a monitor is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Hyprland,
    Sway,
    Kde,
    Gnome,
    /// Another Wayland compositor: no known interface to bind the tablet.
    Wayland,
    X11,
    Fallback,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Hyprland => "Hyprland",
            Backend::Sway => "sway",
            Backend::Kde => "KDE Plasma",
            Backend::Gnome => "GNOME",
            Backend::Wayland => "Wayland",
            Backend::X11 => "Xorg",
            Backend::Fallback => "?",
        }
    }

    /// Can kerntablet bind the tablet to a monitor here by itself?
    pub fn can_bind(self) -> bool {
        !matches!(self, Backend::Wayland | Backend::Fallback)
    }

    /// The Wayland compositor (or Xorg) we run under, from the environment.
    pub fn from_env() -> Option<Backend> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let wayland = var("WAYLAND_DISPLAY").is_some();
        let desktops = var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
        let has = |d: &str| desktops.split(':').any(|x| x == d);
        if var("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            Some(Backend::Hyprland)
        } else if !wayland {
            var("DISPLAY").map(|_| Backend::X11)
        } else if var("SWAYSOCK").is_some() {
            Some(Backend::Sway)
        } else if has("kde") {
            Some(Backend::Kde)
        } else if has("gnome") {
            Some(Backend::Gnome)
        } else {
            Some(Backend::Wayland)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Desktop {
    pub backend: Backend,
    pub width: f64,
    pub height: f64,
    pub monitors: Vec<Monitor>,
}

impl Desktop {
    pub fn fallback() -> Self {
        Self {
            backend: Backend::Fallback,
            width: 1920.0,
            height: 1080.0,
            monitors: vec![Monitor { name: "?".into(), x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 }],
        }
    }
}

/// The rectangle the compositor stretches the tablet's full axis range over.
/// `output: None` means the whole desktop (no output binding).
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub output: Option<String>,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Desktop {
    pub fn whole(&self) -> Target {
        Target { output: None, x: 0.0, y: 0.0, width: self.width, height: self.height }
    }

    /// The monitor that fully contains the display area (half a pixel of slack).
    pub fn monitor_containing(&self, d: &DisplayArea) -> Option<&Monitor> {
        const EPS: f64 = 0.5;
        self.monitors.iter().find(|m| {
            d.x - d.width / 2.0 >= m.x - EPS
                && d.y - d.height / 2.0 >= m.y - EPS
                && d.x + d.width / 2.0 <= m.x + m.width + EPS
                && d.y + d.height / 2.0 <= m.y + m.height + EPS
        })
    }

    /// Where the tablet should be bound for these settings: the monitor that
    /// holds the display area (so the device's coordinate range *is* that
    /// monitor — what Wine/osu! raw input expects), else the whole desktop.
    /// In `auto` mode only where kerntablet can do the binding itself.
    pub fn target_for(&self, cfg: &Config) -> Target {
        let usable = match cfg.bind_output {
            BindMode::Off => false,
            BindMode::Auto => self.backend.can_bind(),
            BindMode::Manual => self.backend != Backend::Fallback,
        };
        if !cfg.enabled || !usable {
            return self.whole();
        }
        match self.monitor_containing(&cfg.display) {
            Some(m) if self.monitors.len() > 1 => Target {
                output: Some(m.name.clone()),
                x: m.x,
                y: m.y,
                width: m.width,
                height: m.height,
            },
            _ => self.whole(),
        }
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn detect() -> Result<Desktop, String> {
    match Backend::from_env() {
        Some(Backend::X11) => parse_xrandr(&run("xrandr", &["--current"])?),
        Some(backend) => bounding_box(backend, wayland::outputs()?, None),
        None => Err("no graphical session found (WAYLAND_DISPLAY / DISPLAY are not set)".into()),
    }
}

/// Logical monitor layout straight from the compositor.
mod wayland {
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{wl_output, wl_registry};
    use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
    use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
    use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};

    use super::Monitor;

    #[derive(Default)]
    struct Out {
        name: Option<String>,
        pos: (i32, i32),
        mode: Option<(i32, i32)>,
        scale: i32,
        rotated: bool,
        logical_pos: Option<(i32, i32)>,
        logical_size: Option<(i32, i32)>,
    }

    impl Out {
        fn monitor(&self, index: usize) -> Option<Monitor> {
            let (x, y) = self.logical_pos.unwrap_or(self.pos);
            let (w, h) = match self.logical_size {
                Some(s) => s,
                // No xdg-output: physical mode / integer scale, rotated.
                None => {
                    let (mut w, mut h) = self.mode?;
                    if self.rotated {
                        std::mem::swap(&mut w, &mut h);
                    }
                    let s = self.scale.max(1);
                    (w / s, h / s)
                }
            };
            if w <= 0 || h <= 0 {
                return None;
            }
            Some(Monitor {
                name: self.name.clone().unwrap_or_else(|| format!("output-{index}")),
                x: x as f64,
                y: y as f64,
                width: w as f64,
                height: h as f64,
            })
        }
    }

    struct State {
        outs: Vec<Out>,
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_output::WlOutput, usize> for State {
        fn event(
            state: &mut Self,
            _: &wl_output::WlOutput,
            event: wl_output::Event,
            idx: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let o = &mut state.outs[*idx];
            match event {
                wl_output::Event::Geometry { x, y, transform, .. } => {
                    o.pos = (x, y);
                    o.rotated = matches!(transform, WEnum::Value(t) if (t as u32) % 2 == 1);
                }
                wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. }
                    if f.contains(wl_output::Mode::Current) =>
                {
                    o.mode = Some((width, height))
                }
                wl_output::Event::Scale { factor } => o.scale = factor,
                wl_output::Event::Name { name } => o.name = Some(name),
                _ => {}
            }
        }
    }

    impl Dispatch<ZxdgOutputManagerV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZxdgOutputManagerV1,
            _: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZxdgOutputV1, usize> for State {
        fn event(
            state: &mut Self,
            _: &ZxdgOutputV1,
            event: zxdg_output_v1::Event,
            idx: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let o = &mut state.outs[*idx];
            match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
                zxdg_output_v1::Event::LogicalSize { width, height } => o.logical_size = Some((width, height)),
                zxdg_output_v1::Event::Name { name } => o.name = Some(name),
                _ => {}
            }
        }
    }

    pub fn outputs() -> Result<Vec<Monitor>, String> {
        let err = |e: &dyn std::fmt::Display| format!("Wayland: {e}");
        let conn = Connection::connect_to_env().map_err(|e| err(&e))?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| err(&e))?;
        let qh = queue.handle();
        let manager: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
        let wl_outputs: Vec<(u32, u32)> = globals.contents().with_list(|list| {
            list.iter().filter(|g| g.interface == "wl_output").map(|g| (g.name, g.version)).collect()
        });
        let mut state = State { outs: Vec::new() };
        // Keep the proxies alive until the roundtrips are done.
        let mut proxies = Vec::new();
        for (i, (name, version)) in wl_outputs.into_iter().enumerate() {
            state.outs.push(Out { scale: 1, ..Default::default() });
            let o: wl_output::WlOutput = globals.registry().bind(name, version.min(4), &qh, i);
            if let Some(m) = &manager {
                proxies.push(m.get_xdg_output(&o, &qh, i));
            }
        }
        queue.roundtrip(&mut state).map_err(|e| err(&e))?;
        queue.roundtrip(&mut state).map_err(|e| err(&e))?;
        drop(proxies);
        Ok(state.outs.iter().enumerate().filter_map(|(i, o)| o.monitor(i)).collect())
    }
}

/// `xrandr --current`
pub fn parse_xrandr(text: &str) -> Result<Desktop, String> {
    let mut screen = None;
    let mut mons = Vec::new();
    for line in text.lines() {
        if line.starts_with("Screen ") {
            // Screen 0: minimum 8 x 8, current 3840 x 1080, maximum 32767 x 32767
            if let Some(cur) = line.split(',').find_map(|p| p.trim().strip_prefix("current ")) {
                let mut it = cur.split('x').map(|s| s.trim().parse::<f64>());
                if let (Some(Ok(w)), Some(Ok(h))) = (it.next(), it.next()) {
                    screen = Some((w, h));
                }
            }
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(name), Some("connected")) = (words.next(), words.next()) else { continue };
        if let Some((w, h, x, y)) = words.find_map(parse_geometry) {
            mons.push(Monitor { name: name.to_string(), x, y, width: w, height: h });
        }
    }
    bounding_box(Backend::X11, mons, screen)
}

/// `1920x1080+1920+0`
fn parse_geometry(tok: &str) -> Option<(f64, f64, f64, f64)> {
    let (size, pos) = tok.split_once('+')?;
    let (w, h) = size.split_once('x')?;
    let (x, y) = pos.split_once('+')?;
    Some((w.parse().ok()?, h.parse().ok()?, x.parse().ok()?, y.parse().ok()?))
}

pub(crate) fn bounding_box(
    backend: Backend,
    mut mons: Vec<Monitor>,
    x_screen: Option<(f64, f64)>,
) -> Result<Desktop, String> {
    if mons.is_empty() {
        return Err("no active monitor found".into());
    }
    let (ox, oy, w, h) = match x_screen {
        // The X server maps absolute devices onto the whole X screen.
        Some((w, h)) => (0.0, 0.0, w, h),
        None => {
            let min_x = mons.iter().map(|m| m.x).fold(f64::INFINITY, f64::min);
            let min_y = mons.iter().map(|m| m.y).fold(f64::INFINITY, f64::min);
            let max_x = mons.iter().map(|m| m.x + m.width).fold(f64::NEG_INFINITY, f64::max);
            let max_y = mons.iter().map(|m| m.y + m.height).fold(f64::NEG_INFINITY, f64::max);
            (min_x, min_y, max_x - min_x, max_y - min_y)
        }
    };
    for m in &mut mons {
        m.x -= ox;
        m.y -= oy;
    }
    mons.sort_by(|a, b| (a.x, a.y).partial_cmp(&(b.x, b.y)).unwrap_or(std::cmp::Ordering::Equal));
    Ok(Desktop { backend, width: w, height: h, monitors: mons })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, x: f64, y: f64, width: f64, height: f64) -> Monitor {
        Monitor { name: name.into(), x, y, width, height }
    }

    fn two_monitors() -> Desktop {
        bounding_box(
            Backend::Hyprland,
            vec![mon("DP-1", 0.0, 0.0, 1920.0, 1080.0), mon("HDMI-A-1", -1920.0, 0.0, 1920.0, 1080.0)],
            None,
        )
        .unwrap()
    }

    #[test]
    fn target_is_the_monitor_holding_the_area() {
        let d = two_monitors();
        let mut cfg = crate::config::parse_otd(
            r#"{"Profiles":[{"AbsoluteModeSettings":{
            "Display":{"Width":1920,"Height":1080,"X":2880,"Y":540},
            "Tablet":{"Width":58,"Height":39.55,"X":123,"Y":75.225,"Rotation":180}}}]}"#,
        )
        .unwrap();
        let t = d.target_for(&cfg);
        assert_eq!(t.output.as_deref(), Some("DP-1"));
        assert_eq!((t.x, t.y, t.width, t.height), (1920.0, 0.0, 1920.0, 1080.0));

        cfg.display.width = 3840.0;
        cfg.display.x = 1920.0;
        assert_eq!(d.target_for(&cfg), d.whole());

        cfg.display.width = 1920.0;
        cfg.display.x = 2880.0;
        cfg.bind_output = BindMode::Off;
        assert_eq!(d.target_for(&cfg), d.whole());
    }

    #[test]
    fn layout_with_negative_origin() {
        let d = two_monitors();
        assert_eq!((d.width, d.height), (3840.0, 1080.0));
        assert_eq!(d.monitors[0].name, "HDMI-A-1");
        assert_eq!(d.monitors[0].x, 0.0);
        assert_eq!(d.monitors[1].name, "DP-1");
        assert_eq!(d.monitors[1].x, 1920.0);
    }

    #[test]
    fn modes_follow_the_backend() {
        let mut d = two_monitors();
        let mut cfg = crate::config::Config::default_for(&crate::tablet::TABLETS[0], &d);
        cfg.bind_output = BindMode::Auto;
        assert!(d.target_for(&cfg).output.is_some());
        d.backend = Backend::Wayland; // no known way to bind it ourselves
        assert_eq!(d.target_for(&cfg), d.whole());
        cfg.bind_output = BindMode::Manual; // ... but the user did it
        assert_eq!(d.target_for(&cfg).output.as_deref(), Some("HDMI-A-1"));
    }

    #[test]
    fn xrandr_layout() {
        let text = "Screen 0: minimum 8 x 8, current 3840 x 1080, maximum 32767 x 32767\n\
DP-0 connected primary 1920x1080+1920+0 (normal left inverted right x axis y axis) 527mm x 296mm\n\
   1920x1080     60.00 +  239.76*\n\
HDMI-0 connected 1920x1080+0+0 (normal left inverted right x axis y axis) 531mm x 299mm\n\
DP-1 disconnected (normal left inverted right x axis y axis)\n\
DP-2 connected (normal left inverted right x axis y axis)\n";
        let d = parse_xrandr(text).unwrap();
        assert_eq!((d.width, d.height), (3840.0, 1080.0));
        assert_eq!(d.monitors.len(), 2);
        assert_eq!(d.monitors[0].name, "HDMI-0");
        assert_eq!(d.monitors[1].name, "DP-0");
        assert_eq!(d.monitors[1].x, 1920.0);
    }
}
