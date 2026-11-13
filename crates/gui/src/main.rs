//! kerntablet — area editor for the in-kernel tablet transform.
//!
//!   kerntablet            open the settings window
//!   kerntablet apply      re-apply saved settings (session autostart, monitor change)
//!   kerntablet status     show what the kernel is doing
//!   kerntablet import-otd take the area from OpenTabletDriver's settings
//!   kerntablet plugin …   list / install / remove / enable / disable plugins

mod app;
mod core;

use std::io::IsTerminal;
use std::process::ExitCode;

use kerntablet_common::bpfmap;
use kerntablet_common::config::{self, Config};
use kerntablet_common::desktop;
use kerntablet_common::plugin::{self, PluginSettings};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => match app::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kerntablet: {e}");
                ExitCode::FAILURE
            }
        },
        ["apply"] => cli_apply(None, false),
        ["apply", "--settle"] => cli_apply(None, true),
        ["import-otd"] => match config::import_otd() {
            Some(cfg) => cli_apply(Some(cfg), false),
            None => {
                eprintln!("kerntablet: no OpenTabletDriver profile found (~/.config/OpenTabletDriver/settings.json)");
                ExitCode::FAILURE
            }
        },
        ["status"] => cli_status(),
        ["plugin", rest @ ..] => cli_plugin(rest),
        _ => {
            eprintln!(
                "kerntablet — tablet area settings; the transform runs in the kernel\n\n\
                 kerntablet             open the settings window\n\
                 kerntablet apply       re-apply saved settings (e.g. after a monitor change)\n\
                 kerntablet apply --settle  same, for session autostart: keeps watching the monitor\n\
                 \x20                      layout for a few seconds and re-applies if it changes\n\
                 kerntablet status      driver status\n\
                 kerntablet import-otd  take the area (and plugin settings) from OpenTabletDriver and apply it\n\
                 kerntablet plugin list                  installed and available plugins\n\
                 kerntablet plugin install <id|dir>      install from the repository or a local directory\n\
                 kerntablet plugin remove <id>\n\
                 kerntablet plugin enable|disable <id>   switch a plugin on/off and apply"
            );
            ExitCode::from(2)
        }
    }
}

fn cli_apply(replace: Option<Config>, settle: bool) -> ExitCode {
    // At session start the monitor layout (and the tablet itself) may still be
    // coming up, so `--settle` keeps re-checking for a few seconds.
    let rounds = if settle { 14 } else { 1 };
    let mut last_desktop = None;
    let mut result = Err("could not detect monitors".to_string());
    for round in 0..rounds {
        if round > 0 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let desktop = match desktop::detect() {
            Ok(d) => d,
            Err(e) => {
                result = Err(e);
                continue;
            }
        };
        if result.is_ok() && last_desktop.as_ref() == Some(&desktop) {
            continue;
        }
        let preferred = replace.and_then(|c| c.preferred).or_else(core::saved_preference);
        let probe = core::probe(preferred);
        let cfg = match replace {
            Some(mut c) => {
                c.preferred = c.preferred.or(preferred);
                c.sanitize(probe.spec);
                c
            }
            None => core::initial_config(probe.spec, &desktop).0,
        };
        result = core::apply(&cfg, &probe, &desktop, true).and_then(|msg| {
            let plugins = plugin::installed().0;
            let mut ps = core::plugin_settings(&plugins);
            if replace.is_some() {
                if let Some(otd) = config::read_otd() {
                    for p in &plugins {
                        if let Some(e) = p.import_otd(&otd) {
                            ps.plugin.insert(p.id().to_string(), e);
                        }
                    }
                }
            }
            core::apply_plugins(&ps, &plugins, &probe, true).map(|extra| msg + &extra)
        });
        last_desktop = Some(desktop);
    }
    match result {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kerntablet: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cli_status() -> ExitCode {
    let probe = core::probe(core::saved_preference());
    println!("Tablet:   {}", probe.device.as_ref().map_or("not connected".to_string(), |d| {
        format!("{} ({}, kernel driver: {})", probe.spec.name, d.name, d.driver.as_deref().unwrap_or("none"))
    }));
    for (d, spec) in probe.connected.iter().filter(|(d, _)| Some(&d.name) != probe.device.as_ref().map(|x| &x.name)) {
        println!("          also connected: {} ({}) — {}", spec.name, d.name, core::health_of(d, spec).describe(spec));
    }
    println!("Status:   {}", probe.health.describe(probe.spec));
    if let Some(dev) = &probe.device {
        if let Ok(st) = bpfmap::StateReader::open(&dev.name).and_then(|r| r.read()) {
            println!("Pen reports processed in kernel: {}", st.events);
        }
    }
    let installed = plugin::installed().0;
    let ps = core::plugin_settings(&installed);
    for p in &installed {
        let on = ps.plugin.get(p.id()).is_some_and(|e| e.enabled);
        let loaded = probe.device.as_ref().is_some_and(|d| bpfmap::plugin_attached(&d.name, p.id()));
        println!(
            "Plugin:   {} {} — {}, {}",
            p.manifest.name,
            p.manifest.version,
            if on { "enabled" } else { "disabled" },
            if loaded { "loaded in kernel" } else { "not loaded (replug the tablet)" }
        );
    }
    match desktop::detect() {
        Ok(d) => {
            println!("Session:  {} — desktop {:.0}×{:.0}", d.backend.label(), d.width, d.height);
            for m in &d.monitors {
                println!("  {:<10} {:.0}×{:.0} @ {:.0},{:.0}", m.name, m.width, m.height, m.x, m.y);
            }
        }
        Err(e) => println!("Session:  {e}"),
    }
    match Config::load() {
        Ok(Some(c)) => println!(
            "Settings: tablet {:.2}×{:.2} mm @ {:.2},{:.2} ({:.0}°) → screen {:.0}×{:.0} @ {:.0},{:.0}",
            c.tablet.width, c.tablet.height, c.tablet.x, c.tablet.y, c.tablet.rotation,
            c.display.width, c.display.height, c.display.x, c.display.y
        ),
        Ok(None) => println!("Settings: not saved yet"),
        Err(e) => println!("Settings: read error — {e}"),
    }
    if probe.health.is_active() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn cli_plugin(args: &[&str]) -> ExitCode {
    let terminal = std::io::stdin().is_terminal();
    let settings = PluginSettings::load().unwrap_or_default();
    let repo = settings.repository().to_string();
    let res = match args {
        ["list"] | [] => {
            let (installed, broken) = plugin::installed();
            let ps = core::plugin_settings(&installed);
            println!("Installed ({}):", plugin::plugin_dir().display());
            if installed.is_empty() {
                println!("  none");
            }
            for p in &installed {
                let on = ps.plugin.get(p.id()).is_some_and(|e| e.enabled);
                println!("  {:<24} {:<8} {}", p.id(), p.manifest.version, if on { "enabled" } else { "disabled" });
            }
            for e in broken {
                println!("  broken: {e}");
            }
            println!("Available ({repo}):");
            match plugin::fetch_index(&repo) {
                Ok(idx) => {
                    for e in idx.plugins {
                        let mark = match installed.iter().find(|p| p.id() == e.id) {
                            Some(p) if p.manifest.version == e.version => "installed",
                            Some(_) => "update available",
                            None => "",
                        };
                        println!("  {:<24} {:<8} {mark}\n      {}", e.id, e.version, e.description);
                    }
                    Ok(String::new())
                }
                Err(e) => Err(e),
            }
        }
        ["install", what] => core::install_plugin(what, &repo, terminal),
        ["remove", id] => core::remove_plugin(id, terminal),
        ["enable", id] | ["disable", id] => {
            let installed = plugin::installed().0;
            if !installed.iter().any(|p| p.id() == *id) {
                eprintln!("kerntablet: plugin {id} is not installed");
                return ExitCode::FAILURE;
            }
            let mut ps = core::plugin_settings(&installed);
            if let Some(e) = ps.plugin.get_mut(*id) {
                e.enabled = args[0] == "enable";
            }
            let probe = core::probe(core::saved_preference());
            core::apply_plugins(&ps, &installed, &probe, true).map(|_| format!("{id}: {}d", args[0]))
        }
        _ => {
            eprintln!("usage: kerntablet plugin [list | install <id|dir> | remove <id> | enable <id> | disable <id>]");
            return ExitCode::from(2);
        }
    };
    match res {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kerntablet: {e}");
            ExitCode::FAILURE
        }
    }
}
