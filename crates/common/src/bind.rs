//! Binds the tablet to one output in the compositor — the same thing
//! `device { output = DP-1 }` did for OpenTabletDriver's virtual tablet.
//!
//! Without it the compositor stretches the device range over the whole
//! desktop and the kernel transform can only *crop* that range, so programs
//! that read absolute device coordinates themselves (Wine raw input, osu!)
//! see the area as a slice of a multi-monitor "virtual screen". Bound to the
//! output, the device's full range is that one monitor.
//!
//! There is no Wayland protocol for this, every compositor has its own knob.
//! kerntablet only uses each one's official runtime/settings interface and
//! never edits the user's compositor config:
//!
//! | Session  | Interface                                                  | Survives          |
//! |----------|------------------------------------------------------------|-------------------|
//! | Hyprland | `hyprctl eval hl.device{…}` / `hyprctl keyword device[…]`  | until config reload (opt-in snippet) |
//! | sway     | `swaymsg input <id> map_to_output <out>`                   | until config reload (opt-in snippet) |
//! | KDE      | KWin D-Bus `org.kde.KWin.InputDevice.outputName`           | yes (KWin saves it)  |
//! | GNOME    | gsettings `org.gnome.desktop.peripherals.tablet output`    | yes (same key as Settings → Wacom) |
//! | Xorg     | `xinput map-to-output`                                     | until the device reappears |

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config::config_home;
use crate::desktop::Backend;
use crate::tablet::TabletInputs;

/// Binds the tablet to `output`, or (`None`) maps it back onto the whole
/// desktop. `Ok` may carry a note for the user.
pub fn bind(backend: Backend, tablet: &TabletInputs, output: Option<&str>) -> Result<String, String> {
    let note = match backend {
        Backend::Hyprland => hyprland(tablet, output)?,
        Backend::Sway => sway(tablet, output)?,
        Backend::Kde => kde(tablet, output)?,
        Backend::Gnome => gnome(tablet, output)?,
        Backend::X11 => x11(tablet, output)?,
        Backend::Wayland | Backend::Fallback => {
            return Err("this compositor has no known interface for binding a tablet to a monitor".into())
        }
    };
    record(backend, output);
    Ok(note)
}

// ------------------------------------------------- what kerntablet bound

fn record_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    base.join("kerntablet").join("bound-output")
}

/// Remembers that kerntablet (not the user) bound the tablet, so turning the
/// binding off later undoes only kerntablet's own change.
fn record(backend: Backend, output: Option<&str>) {
    let path = record_path();
    match output {
        Some(o) => {
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::write(&path, format!("{}\n{o}\n", backend.label()));
        }
        None => {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Did kerntablet bind the tablet in this kind of session?
pub fn bound_by_us(backend: Backend) -> bool {
    fs::read_to_string(record_path()).is_ok_and(|t| t.lines().next() == Some(backend.label()))
}

// ---------------------------------------------------------------- helpers

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
        return Err(format!("{cmd}: {msg}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Writes one of kerntablet's own files, only when it changes.
fn write_if_changed(path: &PathBuf, text: &str) -> Result<(), String> {
    if fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// A double-quoted string for Lua / Hyprland / sway.
fn quoted(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                o.push('\\');
                o.push(c);
            }
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A single-quoted GVariant string.
fn gvariant_str(s: &str) -> String {
    let mut o = String::from("'");
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            o.push('\\');
        }
        o.push(c);
    }
    o.push('\'');
    o
}

// ---------------------------------------------------------------- Hyprland

/// Snippets the user may source from their own config so the binding
/// survives `hyprctl reload`. kerntablet writes them, never the config.
pub fn hyprland_snippets() -> [PathBuf; 2] {
    let dir = config_home().join("kerntablet");
    [dir.join("hyprland.lua"), dir.join("hyprland.conf")]
}

/// The line for the user's Hyprland config (Lua, classic).
pub fn hyprland_source_lines() -> [String; 2] {
    [
        r#"pcall(dofile, os.getenv("HOME") .. "/.config/kerntablet/hyprland.lua")"#.to_string(),
        "source = ~/.config/kerntablet/hyprland.conf".to_string(),
    ]
}

fn hyprland_snippet_sourced() -> bool {
    let dir = config_home().join("hypr");
    ["hyprland.lua", "hyprland.conf"].iter().any(|f| {
        fs::read_to_string(dir.join(f)).is_ok_and(|t| {
            t.lines().any(|l| {
                let l = l.trim_start();
                !l.starts_with("--") && !l.starts_with('#') && l.contains("kerntablet/hyprland.")
            })
        })
    })
}

fn hyprland(tablet: &TabletInputs, output: Option<&str>) -> Result<String, String> {
    let json = run("hyprctl", &["-j", "devices"])?;
    let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| format!("hyprctl devices: {e}"))?;
    let names: Vec<String> = v
        .get("tablets")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .filter(|n| tablet.matches_name(n))
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return Err("Hyprland does not see the tablet pen".into());
    }
    let out = output.unwrap_or("");
    let mut lua = String::from("-- Generated by kerntablet (kerntablet apply), do not edit.\n");
    let mut conf = String::from("# Generated by kerntablet (kerntablet apply), do not edit.\n");
    for n in &names {
        let call = format!("hl.device({{ name = {}, output = {} }})", quoted(n), quoted(out));
        if let Err(e) = run("hyprctl", &["eval", &call]) {
            // Classic hyprland.conf: no Lua `eval`.
            run("hyprctl", &["keyword", &format!("device[{n}]:output"), out]).map_err(|e2| format!("{e}; {e2}"))?;
        }
        lua += &call;
        lua.push('\n');
        conf += &format!("device {{\n    name = {n}\n    output = {out}\n}}\n");
    }
    let [lua_path, conf_path] = hyprland_snippets();
    write_if_changed(&lua_path, &lua)?;
    write_if_changed(&conf_path, &conf)?;
    Ok(if output.is_some() && !hyprland_snippet_sourced() {
        format!(
            " (Hyprland forgets the binding on config reload; to keep it, add to your config: {}  or  {} — then run kerntablet apply again)",
            hyprland_source_lines()[0],
            hyprland_source_lines()[1]
        )
    } else {
        String::new()
    })
}

// ---------------------------------------------------------------- sway

pub fn sway_snippet() -> PathBuf {
    config_home().join("kerntablet").join("sway.conf")
}

/// `swaymsg -t get_inputs -r` -> identifiers of our pen.
fn sway_inputs(json: &str, tablet: &TabletInputs) -> Result<Vec<String>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("swaymsg: {e}"))?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|d| d.get("type").and_then(|t| t.as_str()) == Some("tablet_tool"))
        .filter(|d| {
            let id = |k: &str| d.get(k).and_then(|x| x.as_u64());
            let by_id = id("vendor") == Some(tablet.vid as u64) && id("product") == Some(tablet.pid as u64);
            by_id || d.get("name").and_then(|n| n.as_str()).is_some_and(|n| tablet.matches_name(n))
        })
        .filter_map(|d| d.get("identifier").and_then(|i| i.as_str()).map(str::to_string))
        .collect())
}

fn sway(tablet: &TabletInputs, output: Option<&str>) -> Result<String, String> {
    let ids = sway_inputs(&run("swaymsg", &["-t", "get_inputs", "-r"])?, tablet)?;
    if ids.is_empty() {
        return Err("sway does not see the tablet pen".into());
    }
    // `*` maps the tool onto the whole layout again.
    let out = output.unwrap_or("*");
    let mut conf = String::from("# Generated by kerntablet (kerntablet apply), do not edit.\n");
    for id in &ids {
        let cmd = format!("input {} map_to_output {}", quoted(id), quoted(out));
        run("swaymsg", &[&cmd])?;
        conf += &cmd;
        conf.push('\n');
    }
    write_if_changed(&sway_snippet(), &conf)?;
    Ok(if output.is_some() {
        " (sway forgets the binding on config reload; to keep it, add `include ~/.config/kerntablet/sway.conf` to your config)"
            .to_string()
    } else {
        String::new()
    })
}

// ---------------------------------------------------------------- KDE

/// Sets `outputName` of KWin's input device; KWin stores it in kcminputrc
/// itself, exactly as System Settings → Drawing Tablet does.
fn kde(tablet: &TabletInputs, output: Option<&str>) -> Result<String, String> {
    let events: Vec<&str> = tablet.nodes.iter().filter_map(|n| n.event.as_deref()).collect();
    if events.is_empty() {
        return Err("the tablet has no input device".into());
    }
    let out = output.unwrap_or("");
    let mut done = 0;
    let mut last_err = String::new();
    for ev in events {
        let path = format!("/org/kde/KWin/InputDevice/{ev}");
        let r = run(
            "busctl",
            &["--user", "set-property", "org.kde.KWin", &path, "org.kde.KWin.InputDevice", "outputName", "s", out],
        )
        .or_else(|e| {
            run(
                "gdbus",
                &[
                    "call", "--session", "--dest", "org.kde.KWin", "--object-path", &path,
                    "--method", "org.freedesktop.DBus.Properties.Set",
                    "org.kde.KWin.InputDevice", "outputName", &format!("<{}>", gvariant_str(out)),
                ],
            )
            .map_err(|e2| format!("{e}; {e2}"))
        });
        match r {
            Ok(_) => done += 1,
            Err(e) => last_err = e,
        }
    }
    if done == 0 {
        return Err(format!("KWin: {last_err}"));
    }
    Ok(String::new())
}

// ---------------------------------------------------------------- GNOME

/// `(connector, vendor, product, serial)` of every monitor, from Mutter's
/// `DisplayConfig.GetCurrentState` printed by gdbus.
fn mutter_monitors(text: &str) -> Vec<[String; 4]> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("(('") {
        rest = &rest[i + 2..];
        let mut fields = Vec::new();
        let mut s = rest;
        while fields.len() < 4 {
            let Some(body) = s.strip_prefix('\'') else { break };
            let mut val = String::new();
            let mut end = None;
            let mut chars = body.char_indices();
            while let Some((j, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            val.push(n);
                        }
                    }
                    '\'' => {
                        end = Some(j + 1);
                        break;
                    }
                    c => val.push(c),
                }
            }
            let Some(end) = end else { break };
            fields.push(val);
            s = body[end..].trim_start_matches([',', ' ']);
        }
        if let Ok(f) = <[String; 4]>::try_from(fields) {
            if !found.iter().any(|x: &[String; 4]| x[0] == f[0]) {
                found.push(f);
            }
        }
    }
    found
}

/// Number of strings in a printed `as` value (`['a', 'b', '']` -> 3).
fn gvariant_strv_len(text: &str) -> usize {
    let t = text.trim().trim_start_matches("@as").trim();
    let t = t.trim_start_matches('[').trim_end_matches(']').trim();
    if t.is_empty() {
        0
    } else {
        // Strings may contain commas: count top-level quotes instead.
        let mut n = 0;
        let mut in_str = None;
        let mut esc = false;
        for c in t.chars() {
            match (in_str, c) {
                (Some(_), _) if esc => esc = false,
                (Some(_), '\\') => esc = true,
                (Some(q), c) if c == q => in_str = None,
                (None, '\'' | '"') => {
                    in_str = Some(c);
                    n += 1;
                }
                _ => {}
            }
        }
        n
    }
}

/// The same key GNOME Settings → Wacom Tablet → "Map to monitor" writes.
fn gnome(tablet: &TabletInputs, output: Option<&str>) -> Result<String, String> {
    let schema = format!(
        "org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/{:04x}:{:04x}/",
        tablet.vid, tablet.pid
    );
    // Older GNOME: [vendor, product, serial]; newer adds the connector.
    let len = gvariant_strv_len(&run("gsettings", &["get", &schema, "output"])?).max(3);
    let value: Vec<String> = match output {
        None => vec![String::new(); len],
        Some(out) => {
            let state = run(
                "gdbus",
                &[
                    "call", "--session", "--dest", "org.gnome.Mutter.DisplayConfig",
                    "--object-path", "/org/gnome/Mutter/DisplayConfig",
                    "--method", "org.gnome.Mutter.DisplayConfig.GetCurrentState",
                ],
            )?;
            let m = mutter_monitors(&state)
                .into_iter()
                .find(|m| m[0] == out)
                .ok_or_else(|| format!("GNOME does not list monitor {out}"))?;
            let [connector, vendor, product, serial] = m;
            let mut v = vec![vendor, product, serial];
            if len >= 4 {
                v.push(connector);
            }
            v
        }
    };
    let arg = format!("[{}]", value.iter().map(|s| gvariant_str(s)).collect::<Vec<_>>().join(", "));
    run("gsettings", &["set", &schema, "output", &arg])?;
    Ok(String::new())
}

// ---------------------------------------------------------------- Xorg

/// `xinput list` -> ids of our pointer devices.
fn x11_devices(list: &str, tablet: &TabletInputs) -> Vec<String> {
    list.lines()
        .filter(|l| l.contains("slave  pointer"))
        .filter_map(|l| {
            let (name, rest) = l.split_once("id=")?;
            tablet.matches_name(name.trim()).then_some(rest)
        })
        .filter_map(|r| r.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

fn x11(tablet: &TabletInputs, output: Option<&str>) -> Result<String, String> {
    let ids = x11_devices(&run("xinput", &["list"])?, tablet);
    if ids.is_empty() {
        return Err("xinput does not see the tablet pen".into());
    }
    for id in &ids {
        match output {
            Some(out) => run("xinput", &["map-to-output", id, out])?,
            None => run(
                "xinput",
                &["set-prop", id, "Coordinate Transformation Matrix", "1", "0", "0", "0", "1", "0", "0", "0", "1"],
            )?,
        };
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tablet::InputNode;

    fn ctl472() -> TabletInputs {
        TabletInputs {
            vid: 0x056a,
            pid: 0x037a,
            nodes: vec![InputNode { name: "Wacom One by Wacom S Pen".into(), event: Some("event2".into()) }],
        }
    }

    #[test]
    fn names_as_compositors_spell_them() {
        let t = ctl472();
        assert!(t.matches_name("wacom-one-by-wacom-s-pen"));
        assert!(t.matches_name("Wacom_One_by_Wacom_S_Pen"));
        assert!(t.matches_name("Wacom One by Wacom S Pen stylus"));
        assert!(!t.matches_name("wacom-intuos-pro-m-pen"));
        assert!(!t.matches_name("Logitech Mouse"));
    }

    #[test]
    fn xinput_ids() {
        let list = "⎡ Virtual core pointer                    \tid=2\t[master pointer  (3)]\n\
⎜   ↳ Wacom One by Wacom S Pen stylus       \tid=10\t[slave  pointer  (2)]\n\
⎜   ↳ Wacom One by Wacom S Pen eraser       \tid=11\t[slave  pointer  (2)]\n\
⎜   ↳ Wacom Intuos Pro M Pen stylus         \tid=13\t[slave  pointer  (2)]\n\
⎜   ↳ Logitech Mouse                        \tid=12\t[slave  pointer  (2)]\n\
⎣ Virtual core keyboard                   \tid=3\t[master keyboard (2)]\n";
        assert_eq!(x11_devices(list, &ctl472()), vec!["10", "11"]);
    }

    #[test]
    fn sway_identifiers() {
        let json = r#"[
          {"identifier":"1386:890:Wacom_One_by_Wacom_S_Pen","name":"Wacom One by Wacom S Pen","vendor":1386,"product":890,"type":"tablet_tool"},
          {"identifier":"1386:890:Wacom_One_by_Wacom_S_Pad","name":"Wacom One by Wacom S Pad","vendor":1386,"product":890,"type":"tablet_pad"},
          {"identifier":"1133:16505:Logitech","name":"Logitech","vendor":1133,"product":16505,"type":"pointer"}]"#;
        assert_eq!(sway_inputs(json, &ctl472()).unwrap(), vec!["1386:890:Wacom_One_by_Wacom_S_Pen"]);
    }

    #[test]
    fn mutter_state() {
        let text = "(uint32 7, [(('DP-1', 'GSM', 'LG ULTRAGEAR', '0x0001b2c3'), [('1920x1080@239.760', 1920, 1080, 239.76, 1.0, [1.0, 2.0], {'is-current': <true>})], {'display-name': <'LG 27\"'>}), \
(('HDMI-1', 'DEL', 'DELL P2419H', 'it\\'s 9'), [], {})], [(0, 0, 1.0, uint32 0, true, [('DP-1', 'GSM', 'LG ULTRAGEAR', '0x0001b2c3')], @a{sv} {})], {})";
        let m = mutter_monitors(text);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], ["DP-1", "GSM", "LG ULTRAGEAR", "0x0001b2c3"].map(String::from));
        assert_eq!(m[1][0], "HDMI-1");
        assert_eq!(m[1][3], "it's 9");
    }

    #[test]
    fn strv_lengths() {
        assert_eq!(gvariant_strv_len("['', '', '']\n"), 3);
        assert_eq!(gvariant_strv_len("['GSM', 'LG, 27', 'x', 'DP-1']"), 4);
        assert_eq!(gvariant_strv_len("@as []"), 0);
    }

    #[test]
    fn escaping() {
        assert_eq!(quoted(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(gvariant_str("it's"), r"'it\'s'");
    }
}
