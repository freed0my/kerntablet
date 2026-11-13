//! User-facing settings. Same conventions as OpenTabletDriver so numbers can
//! be copied over 1:1: positions are the *centre* of an area; the tablet area
//! is in millimetres, the display area in pixels relative to the top-left
//! corner of the whole desktop.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::desktop::Desktop;
use crate::tablet::{TabletId, TabletSpec};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TabletArea {
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
    /// Degrees, same sign convention as OpenTabletDriver.
    #[serde(default)]
    pub rotation: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DisplayArea {
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Keep the tablet area's aspect ratio equal to the display area's.
    #[serde(default)]
    pub lock_aspect: bool,
    /// Clamp the cursor to the display area.
    #[serde(default = "yes")]
    pub clip: bool,
    /// Ignore the pen entirely outside of the tablet area.
    #[serde(default)]
    pub limit: bool,
    /// What to do when the display area lies on one monitor (see [`BindMode`]).
    #[serde(default, deserialize_with = "bind_mode_compat")]
    pub bind_output: BindMode,
    /// Which tablet to configure when several supported ones are plugged in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred: Option<TabletId>,
    pub display: DisplayArea,
    pub tablet: TabletArea,
}

fn yes() -> bool {
    true
}

/// How the tablet gets tied to the monitor the display area is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindMode {
    /// kerntablet binds the tablet to that monitor through the compositor's
    /// own runtime/settings interface, and the kernel matrix spans only it.
    #[default]
    Auto,
    /// The user bound the tablet to that monitor in the compositor
    /// themselves; kerntablet only computes the matrix for it.
    Manual,
    /// No binding: the compositor maps the tablet onto the whole desktop and
    /// the kernel matrix targets the area inside it.
    Off,
}

impl BindMode {
    pub const ALL: [BindMode; 3] = [BindMode::Auto, BindMode::Manual, BindMode::Off];
}

/// `bind_output` used to be a bool: `true` = auto, `false` = off.
fn bind_mode_compat<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BindMode, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Bool(bool),
        Mode(BindMode),
    }
    Ok(match Either::deserialize(d)? {
        Either::Bool(true) => BindMode::Auto,
        Either::Bool(false) => BindMode::Off,
        Either::Mode(m) => m,
    })
}

impl Config {
    /// Whole tablet mapped onto the first monitor.
    pub fn default_for(spec: &TabletSpec, desktop: &Desktop) -> Self {
        let display = match desktop.monitors.first() {
            Some(m) => DisplayArea {
                width: m.width,
                height: m.height,
                x: m.x + m.width / 2.0,
                y: m.y + m.height / 2.0,
            },
            None => DisplayArea {
                width: desktop.width,
                height: desktop.height,
                x: desktop.width / 2.0,
                y: desktop.height / 2.0,
            },
        };
        Self {
            enabled: true,
            lock_aspect: false,
            clip: true,
            limit: false,
            bind_output: BindMode::Auto,
            preferred: None,
            display,
            tablet: TabletArea {
                width: spec.width_mm,
                height: spec.height_mm,
                x: spec.width_mm / 2.0,
                y: spec.height_mm / 2.0,
                rotation: 0.0,
            },
        }
    }

    pub fn path() -> PathBuf {
        config_home().join("kerntablet").join("config.toml")
    }

    pub fn load() -> io::Result<Option<Self>> {
        let text = match fs::read_to_string(Self::path()) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        fs::write(path, text)
    }

    /// Sanity limits so a broken file can never produce a degenerate matrix.
    pub fn sanitize(&mut self, spec: &TabletSpec) {
        let fix = |v: &mut f64, min: f64, max: f64, def: f64| {
            if !v.is_finite() {
                *v = def;
            }
            *v = v.clamp(min, max);
        };
        fix(&mut self.tablet.width, 1.0, spec.width_mm * 4.0, spec.width_mm);
        fix(&mut self.tablet.height, 1.0, spec.height_mm * 4.0, spec.height_mm);
        fix(&mut self.tablet.x, -spec.width_mm, spec.width_mm * 2.0, spec.width_mm / 2.0);
        fix(&mut self.tablet.y, -spec.height_mm, spec.height_mm * 2.0, spec.height_mm / 2.0);
        fix(&mut self.tablet.rotation, -360.0, 360.0, 0.0);
        fix(&mut self.display.width, 1.0, 100_000.0, 1920.0);
        fix(&mut self.display.height, 1.0, 100_000.0, 1080.0);
        fix(&mut self.display.x, -100_000.0, 100_000.0, 960.0);
        fix(&mut self.display.y, -100_000.0, 100_000.0, 540.0);
    }
}

pub fn config_home() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
}

/// Pulls the area settings out of an existing OpenTabletDriver profile.
pub fn import_otd() -> Option<Config> {
    parse_otd(&read_otd()?)
}

/// Raw OpenTabletDriver settings.json, if there is one.
pub fn read_otd() -> Option<String> {
    fs::read_to_string(config_home().join("OpenTabletDriver").join("settings.json")).ok()
}

pub fn parse_otd(text: &str) -> Option<Config> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let abs = v.get("Profiles")?.get(0)?.get("AbsoluteModeSettings")?;
    let num = |obj: &serde_json::Value, key: &str| obj.get(key).and_then(|x| x.as_f64());
    let d = abs.get("Display")?;
    let t = abs.get("Tablet")?;
    let flag = |key: &str, def: bool| abs.get(key).and_then(|x| x.as_bool()).unwrap_or(def);
    Some(Config {
        enabled: true,
        lock_aspect: flag("LockAspectRatio", false),
        clip: flag("EnableClipping", true),
        limit: flag("EnableAreaLimiting", false),
        bind_output: BindMode::Auto,
        preferred: None,
        display: DisplayArea {
            width: num(d, "Width")?,
            height: num(d, "Height")?,
            x: num(d, "X")?,
            y: num(d, "Y")?,
        },
        tablet: TabletArea {
            width: num(t, "Width")?,
            height: num(t, "Height")?,
            x: num(t, "X")?,
            y: num(t, "Y")?,
            rotation: num(t, "Rotation").unwrap_or(0.0),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_otd_profile() {
        let json = r#"{"Profiles":[{"Tablet":"Wacom CTL-472","AbsoluteModeSettings":{
            "Display":{"Width":1920.0,"Height":1080.0,"X":2880.0,"Y":540.0,"Rotation":0.0},
            "Tablet":{"Width":58.0,"Height":39.55,"X":123.0,"Y":75.225,"Rotation":180.0},
            "EnableClipping":true,"EnableAreaLimiting":false,"LockAspectRatio":false}}]}"#;
        let c = parse_otd(json).unwrap();
        assert_eq!(c.display, DisplayArea { width: 1920.0, height: 1080.0, x: 2880.0, y: 540.0 });
        assert_eq!(c.tablet.rotation, 180.0);
        assert_eq!(c.tablet.width, 58.0);
        assert!(c.clip && !c.limit && !c.lock_aspect);
    }

    #[test]
    fn toml_roundtrip() {
        let c = parse_otd(
            r#"{"Profiles":[{"AbsoluteModeSettings":{
            "Display":{"Width":1920,"Height":1080,"X":960,"Y":540},
            "Tablet":{"Width":100,"Height":56.25,"X":76,"Y":47.5,"Rotation":0}}}]}"#,
        )
        .unwrap();
        let text = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
    }

    #[test]
    fn bind_output_accepts_old_bools() {
        let base = "[display]\nwidth = 1\nheight = 1\nx = 0\ny = 0\n[tablet]\nwidth = 1\nheight = 1\nx = 0\ny = 0\n";
        let mode = |line: &str| toml::from_str::<Config>(&format!("{line}\n{base}")).unwrap().bind_output;
        assert_eq!(mode("bind_output = true"), BindMode::Auto);
        assert_eq!(mode("bind_output = false"), BindMode::Off);
        assert_eq!(mode("bind_output = \"manual\""), BindMode::Manual);
        assert_eq!(mode(""), BindMode::Auto);
    }
}
