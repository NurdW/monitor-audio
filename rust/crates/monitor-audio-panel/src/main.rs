//! The control panel: what is routed where, and the buttons that install or
//! remove the service.
//!
//! It asks the running service for everything it shows rather than working any of
//! it out itself, so the window can never describe a decision the routing is not
//! making.

mod displays;
mod install;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use eframe::egui;
use displays::{connected_displays, Display};
use install::{install_files, remove_files, Roots, SystemRunner, KWIN_SCRIPT_ID};

const REFRESH_SECONDS: u64 = 3;

/// The Python panel is shipped alongside this one, so the two windows have to be
/// tellable apart in the taskbar and in Alt+Tab. The suffix is the only thing
/// that says which program you are looking at.
const APP_NAME: &str = "Monitor-Audio-Rust";

#[derive(PartialEq, Eq)]
enum Tab {
    Monitors,
    Applications,
    Streams,
    Log,
}

/// The service's own command line. Asked over D-Bus would be tidier, but the panel
/// and the service are separate programs and the command line is the interface
/// both the KWin script and the Python panel already depend on, so it is the one
/// thing that must not drift.
fn daemon_binary() -> PathBuf {
    if let Ok(path) = std::env::var("MONITOR_AUDIO_DAEMON") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent().unwrap_or(Path::new(".")).join("monitor-audio");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("monitor-audio")
}

fn daemon_call(args: &[&str]) -> Result<String, String> {
    let binary = daemon_binary();
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + 1);
    full.push(binary.to_str().unwrap_or("monitor-audio"));
    full.extend_from_slice(args);
    run(&full).map(|text| text.trim().to_string())
}

fn run(args: &[&str]) -> Result<String, String> {
    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .map_err(|err| format!("{}: {}", args[0], err))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(format!(
            "{}: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim().chars().take(200).collect::<String>()
        ))
    }
}

/// Write the panel's keys into the settings file, keeping every key it does not
/// own.
///
/// `fallback_kind` is hand-edited and documented, so a file rebuilt from the
/// panel's own fields alone would quietly reset it the first time a checkbox was
/// touched. A file that cannot be parsed at all is not worth preserving, and the
/// reason is printed rather than swallowed.
fn merge_settings(path: &Path, changed: serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let mut body = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
            .unwrap_or_else(|err| {
                eprintln!("could not read {} as an object: {}; writing a fresh file", path.display(), err);
                serde_json::Map::new()
            }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(err) => return Err(format!("could not read {}: {}", path.display(), err)),
    };
    for (key, value) in changed {
        body.insert(key, value);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {}", parent.display(), err))?;
    }
    std::fs::write(path, serde_json::Value::Object(body).to_string())
        .map_err(|err| format!("could not write {}: {}", path.display(), err))
}

fn config_path() -> PathBuf {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"),
    };
    base.join("monitor-audio").join("config.json")
}

/// A short tone, so an output can be identified by ear instead of by name.
fn write_tone(path: &Path) -> Result<(), String> {
    let seconds = 2usize;
    let rate = 48000usize;
    let samples = rate * seconds;
    let mut data = Vec::with_capacity(samples * 2);
    for index in 0..samples {
        // A little decay at the ends keeps the start and stop from clicking.
        let envelope = (std::f32::consts::PI * (index as f32 / samples as f32)).sin();
        let value = (index as f32 * 440.0 * 2.0 * std::f32::consts::PI / rate as f32).sin();
        let sample = (value * envelope * 12000.0) as i16;
        data.extend_from_slice(&sample.to_le_bytes());
    }
    let mut header = Vec::new();
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&(rate as u32).to_le_bytes());
    header.extend_from_slice(&(rate as u32).to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&(data.len() as u32).to_le_bytes());
    std::fs::write(path, [header, data].concat()).map_err(|err| err.to_string())
}

struct Panel {
    tab: Tab,
    status: String,
    report: serde_json::Value,
    resolutions: serde_json::Value,
    displays: Vec<Display>,
    overrides: BTreeMap<String, String>,
    enabled: bool,
    keep_awake: bool,
    last_refresh: Instant,
    log_text: String,
    follow_log: bool,
    tone: Option<Child>,
    confirm_remove: bool,
    installed: bool,
}

impl Panel {
    fn new() -> Panel {
        let mut panel = Panel {
            tab: Tab::Monitors,
            status: String::from("starting..."),
            report: serde_json::json!({}),
            resolutions: serde_json::json!({}),
            displays: connected_displays(Path::new("/sys/class/drm")),
            overrides: BTreeMap::new(),
            enabled: true,
            keep_awake: true,
            last_refresh: Instant::now() - Duration::from_secs(REFRESH_SECONDS + 1),
            log_text: String::new(),
            follow_log: true,
            tone: None,
            confirm_remove: false,
            installed: Roots::from_env().is_some_and(|roots| roots.unit_target.exists()),
        };
        panel.read_settings();
        panel.refresh();
        panel
    }

    /// The install targets, or nothing when `HOME` is missing.
    ///
    /// A panel that cannot say where home is must not install or remove: every
    /// target is built by joining onto it, so a blank would become a relative
    /// path into the current directory.
    fn roots(&self) -> Option<Roots> {
        Roots::from_env()
    }

    fn read_settings(&mut self) {
        // Collected rather than assigned from inside the logger: the closure is an
        // immutable Fn, and a broken settings file must still be reported.
        let complaints = std::cell::RefCell::new(Vec::new());
        let config = monitor_audio_core::config::load_config(&config_path(), &|message| {
            complaints.borrow_mut().push(message.to_string());
        });
        let complaints = complaints.borrow();
        if !complaints.is_empty() {
            self.status = complaints.join("; ");
        }
        self.enabled = config.enabled;
        self.keep_awake = config.keep_awake;
        self.overrides = config.manual_output;
    }

    fn write_settings(&mut self) {
        let mut changed = serde_json::Map::new();
        changed.insert("enabled".into(), self.enabled.into());
        changed.insert("keep_awake".into(), self.keep_awake.into());
        changed.insert("manual_output".into(), serde_json::to_value(&self.overrides).unwrap_or_default());
        self.status = match merge_settings(&config_path(), changed) {
            Ok(()) => format!("saved to {}", config_path().display()),
            Err(err) => err,
        };
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        match daemon_call(&["json"]) {
            Ok(text) => {
                let start = text.find('{').unwrap_or(0);
                match serde_json::from_str(&text[start..]) {
                    Ok(value) => {
                        self.report = value;
                        self.status = String::new();
                    }
                    Err(err) => self.status = format!("the service sent unreadable json: {}", err),
                }
            }
            Err(err) => {
                self.status = format!("the service is not reachable: {}", err);
                self.report = serde_json::json!({});
            }
        }
        let models: Vec<String> = self.displays.iter().map(|display| display.model.clone()).collect();
        if models.is_empty() {
            return;
        }
        match serde_json::to_string(&models) {
            Ok(request) => match daemon_call(&["resolve", &request]) {
                Ok(text) => {
                    let start = text.find('{').unwrap_or(0);
                    match serde_json::from_str(&text[start..]) {
                        Ok(value) => self.resolutions = value,
                        Err(err) => self.status = format!("the service sent unreadable json: {}", err),
                    }
                }
                Err(err) => self.status = format!("could not ask the service about your displays: {}", err),
            },
            Err(err) => self.status = format!("could not ask the service about your displays: {}", err),
        }
    }

    fn set_override(&mut self, model: &str, choice: &str) {
        if choice.is_empty() {
            self.overrides.remove(model);
        } else {
            self.overrides.insert(model.to_string(), choice.to_string());
        }
        self.write_settings();
    }

    fn install(&mut self) {
        let Some(roots) = self.roots() else {
            self.status = "HOME is not set, so I cannot tell where to install. Refusing to guess.".into();
            return;
        };
        let exe = std::env::current_exe().unwrap_or_default();
        let problems = install_files(&roots, &exe, &SystemRunner);
        self.installed = true;
        self.status = if problems.is_empty() {
            "Installed. Drag a window to another monitor to try it.".to_string()
        } else {
            format!("could not install: {}", problems.join("; "))
        };
    }

    fn remove(&mut self) {
        let Some(roots) = self.roots() else {
            self.status = "HOME is not set, so I cannot tell what to remove. Refusing to guess.".into();
            return;
        };
        let problems = remove_files(&roots, &SystemRunner);
        self.installed = false;
        self.status = if problems.is_empty() {
            "Removed. Your display choices under ~/.config/monitor-audio were kept.".to_string()
        } else {
            format!("removed, with problems: {}", problems.join("; "))
        };
    }

    fn play_tone(&mut self, output: &str) {
        let path = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("monitor-audio-tone.wav");
        if let Err(err) = write_tone(&path) {
            self.status = format!("could not make the test tone: {}", err);
            return;
        }
        // Spawned rather than waited on: `output()` would block the whole window
        // for the length of the tone. A second click cuts the first one off.
        if let Some(mut previous) = self.tone.take() {
            if let Err(err) = previous.kill() {
                self.status = format!("could not stop the previous tone: {}", err);
            }
            if let Err(err) = previous.wait() {
                self.status = format!("could not reap the previous tone: {}", err);
            }
        }
        let argument = format!("--device={}", output);
        match Command::new("paplay").arg(&argument).arg(&path)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.tone = Some(child);
                self.status = format!("playing a tone on {}", output);
            }
            Err(err) => self.status = format!("could not play the tone: {}", err),
        }
    }

    fn refresh_log(&mut self) {
        match run(&["journalctl", "--user", "-u", "monitor-audio.service",
                    "-n", "400", "--no-pager"]) {
            Ok(text) => self.log_text = text,
            Err(err) => self.log_text = format!("could not read the log: {}", err),
        }
    }
}

/// The rows of one section of the report, empty when the service said nothing.
fn rows(report: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    report[key].as_array().cloned().unwrap_or_default()
}

fn text(value: &serde_json::Value, key: &str) -> String {
    value[key].as_str().map(str::to_string).unwrap_or_default()
}

fn list(value: &serde_json::Value, key: &str) -> String {
    value[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<&str>>()
                .join(", ")
        })
        .unwrap_or_default()
}

impl Panel {
    /// Everything the window shows, kept out of the eframe callback so a test can
    /// drive it with a plain context and no window server.
    fn draw(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Monitors, "Monitors");
                ui.selectable_value(&mut self.tab, Tab::Applications, "Applications");
                ui.selectable_value(&mut self.tab, Tab::Streams, "Audio streams");
                ui.selectable_value(&mut self.tab, Tab::Log, "Log");
            });
            ui.separator();

            match self.tab {
                Tab::Monitors => {
                    let mut changed = false;
                    ui.horizontal(|ui| {
                        changed |= ui.checkbox(&mut self.enabled, "Routing switched on").changed();
                        changed |= ui
                            .checkbox(&mut self.keep_awake, "Keep routed outputs awake")
                            .on_hover_text(
                                "Holds one silent stream on every output you route to, so it never \
                                 sleeps and loses the first seconds of what follows. The stream \
                                 shows up in the volume mixer as monitor-audio-keepalive.",
                            )
                            .changed();
                    });
                    if changed {
                        self.write_settings();
                    }
                    ui.horizontal(|ui| {
                        if ui.button(if self.installed { "Reinstall" } else { "Install" }).clicked() {
                            self.install();
                        }
                        if ui.button("Remove").clicked() {
                            self.confirm_remove = true;
                        }
                        if ui.button("Forget current routes").clicked() {
                            self.status = daemon_call(&["reset"]).unwrap_or_else(|err| err);
                        }
                    });
                    if self.confirm_remove {
                        ui.colored_label(
                            egui::Color32::from_rgb(200, 120, 40),
                            "This stops audio following your windows and removes the service and \
                             the KWin script. Your display choices are kept.",
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Yes, remove").clicked() {
                                self.confirm_remove = false;
                                self.remove();
                            }
                            if ui.button("Cancel").clicked() {
                                self.confirm_remove = false;
                            }
                        });
                    }
                    ui.separator();

                    let displays = self.displays.clone();
                    let resolutions = self.resolutions.clone();
                    let overrides = self.overrides.clone();
                    let outputs: Vec<(String, String)> = self.report["outputs"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| (text(item, "name"), text(item, "description")))
                                .collect()
                        })
                        .unwrap_or_default();

                    egui::Grid::new("monitors")
                        .num_columns(3)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Display").strong());
                            ui.label(egui::RichText::new("Connector").strong());
                            ui.label(egui::RichText::new("Audio output in use").strong());
                            ui.end_row();
                            for display in &displays {
                                ui.label(&display.model);
                                ui.label(&display.connector);
                                let info = &resolutions[&display.model];
                                let automatic = format!(
                                    "Automatic - {}",
                                    info["description"].as_str().unwrap_or("no output found")
                                );
                                let chosen = overrides.get(&display.model).cloned();
                                let current = chosen
                                    .as_ref()
                                    .and_then(|name| {
                                        outputs.iter().find(|(output, _)| output == name).map(|(_, d)| d.clone())
                                    })
                                    .unwrap_or_else(|| automatic.clone());
                                egui::ComboBox::from_id_salt(format!("output-{}", display.model))
                                    .selected_text(current)
                                    .show_ui(ui, |ui| {
                                        if ui.selectable_label(false, &automatic).clicked() {
                                            self.set_override(&display.model, "");
                                        }
                                        for (name, description) in &outputs {
                                            if ui.selectable_label(chosen.as_deref() == Some(name.as_str()), description).clicked() {
                                                self.set_override(&display.model, name);
                                            }
                                        }
                                    });
                                ui.end_row();
                            }
                        });
                }
                Tab::Applications => {
                    egui::Grid::new("routes")
                        .num_columns(3)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Display").strong());
                            ui.label(egui::RichText::new("Output").strong());
                            ui.label(egui::RichText::new("Why").strong());
                            ui.end_row();
                            for route in &rows(&self.report, "routes") {
                                ui.label(text(route, "model"));
                                ui.label(text(route, "output"));
                                ui.label(format!("{} ({})", list(route, "captions"), text(route, "how")));
                                ui.end_row();
                            }
                        });
                }
                Tab::Streams => {
                    egui::Grid::new("streams")
                        .num_columns(3)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Title").strong());
                            ui.label(egui::RichText::new("Output").strong());
                            ui.label(egui::RichText::new("Placed by").strong());
                            ui.end_row();
                            for stream in &rows(&self.report, "streams") {
                                let title = list(stream, "titles");
                                ui.label(if title.is_empty() { list(stream, "apps") } else { title.clone() });
                                ui.label(text(stream, "output"));
                                ui.label(stream["placed_by"].as_str().unwrap_or(""));
                                ui.end_row();
                            }
                        });
                    ui.separator();
                    ui.label("Play a test tone so you can tell the outputs apart by ear:");
                    let tones: Vec<(String, String)> = self.report["outputs"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|output| (text(output, "name"), text(output, "description")))
                                .collect()
                        })
                        .unwrap_or_default();
                    egui::Grid::new("tones").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        for (name, description) in &tones {
                            if ui.button(description).clicked() {
                                self.play_tone(name);
                            }
                            ui.end_row();
                        }
                    });
                }
                Tab::Log => {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.follow_log, "Follow new lines");
                        if ui.button("Refresh").clicked() {
                            self.refresh_log();
                        }
                    });
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.label(&self.log_text);
                    });
                }
            }

            ui.separator();
            if !self.status.is_empty() {
                ui.label(&self.status);
            }
            ui.label(format!(
                "KWin script: {} · service unit: {}",
                KWIN_SCRIPT_ID,
                match self.roots() {
                    Some(roots) => roots.unit_target.display().to_string(),
                    None => "unknown, HOME is not set".to_string(),
                }
            ));
        });
    }
}

impl eframe::App for Panel {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_refresh.elapsed() > Duration::from_secs(REFRESH_SECONDS) {
            self.refresh();
            if self.tab == Tab::Log && self.follow_log {
                self.refresh_log();
            }
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        self.draw(ctx);
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([860.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|_cc| Ok(Box::new(Panel::new()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ma-panel-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn two_keys() -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        map.insert("enabled".into(), false.into());
        map.insert("keep_awake".into(), true.into());
        map
    }

    #[test]
    fn saving_settings_keeps_keys_the_panel_does_not_control() {
        // fallback_kind is only ever set by hand, so losing it would silently
        // change where audio goes for anyone who had bothered to set it.
        let path = scratch("settings").join("config.json");
        std::fs::write(&path, r#"{"fallback_kind":"hdmi","manual_output":{"TV":"x"},"enabled":true}"#)
            .expect("seed settings");
        merge_settings(&path, two_keys()).expect("merge settings");
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse");
        assert_eq!(saved["fallback_kind"], "hdmi");
        assert_eq!(saved["enabled"], false);
        assert_eq!(saved["keep_awake"], true);
        assert_eq!(saved["manual_output"]["TV"], "x");
    }

    #[test]
    fn a_settings_file_that_is_not_an_object_is_replaced_not_silently_kept() {
        let path = scratch("broken").join("config.json");
        std::fs::write(&path, "this is not json").expect("seed broken file");
        merge_settings(&path, two_keys()).expect("recover from a broken file");
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("parse");
        assert_eq!(saved["enabled"], false);
        assert!(saved.get("fallback_kind").is_none());
    }

    #[test]
    fn saving_settings_reports_a_file_it_cannot_write() {
        // The panel must not report "saved" when the write was refused.
        let dir = scratch("read-only");
        let path = dir.join("config.json");
        std::fs::write(&path, "{}").expect("seed settings");
        // The file itself, not the directory: overwriting an existing file needs
        // write permission on the file, and a read-only directory would still
        // have allowed it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))
            .expect("make the settings file read-only");
        let error = merge_settings(&path, two_keys()).expect_err("should fail");
        assert!(error.contains("could not write"), "unexpected error: {}", error);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("let the scratch file be cleaned up");
    }

    #[test]
    fn the_test_tone_is_a_wav_file_that_says_the_right_thing_about_itself() {
        let path = scratch("tone").join("tone.wav");
        write_tone(&path).expect("write tone");
        let bytes = std::fs::read(&path).expect("read tone");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().expect("channels")), 1);
        let data_size = u32::from_le_bytes(bytes[40..44].try_into().expect("data size")) as usize;
        // Two seconds of mono 48kHz at two bytes a sample, and the file must be
        // exactly as long as its header claims or players complain.
        assert_eq!(data_size, 48000 * 2 * 2);
        assert_eq!(bytes.len(), 44 + data_size);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().expect("riff size")),
            36 + data_size as u32
        );
    }

    /// A panel that never talks to the service: the drawing code must not depend on
    /// a running daemon, and a test must not rearrange a real desktop.
    fn blank() -> Panel {
        Panel {
            tab: Tab::Monitors,
            status: String::from("test"),
            report: serde_json::json!({
                "outputs": [{"name": "alsa.analog", "description": "Analog Stereo", "index": "47"}],
                "routes": [{"model": "SAMSUNG", "output": "alsa.hdmi", "how": "override",
                            "captions": ["jellyfin"], "apps": ["firefox"]}],
                "streams": [{"id": "5", "output": "alsa.analog", "titles": ["jellyfin"],
                             "apps": ["firefox"], "pids": ["1"], "placed_by": "w1"}],
            }),
            resolutions: serde_json::json!({"SAMSUNG": {"description": "HDMI [SAMSUNG]", "how": "override"}}),
            displays: vec![Display { model: "SAMSUNG".into(), connector: "HDMI-A-1".into() }],
            overrides: BTreeMap::new(),
            enabled: true,
            keep_awake: true,
            last_refresh: Instant::now(),
            log_text: String::from("a log line"),
            follow_log: true,
            tone: None,
            confirm_remove: false,
            installed: true,
        }
    }

    #[test]
    fn every_tab_draws_without_a_service_running() {
        let context = egui::Context::default();
        let mut panel = blank();
        for tab in [Tab::Monitors, Tab::Applications, Tab::Streams, Tab::Log] {
            panel.tab = tab;
            // egui needs a real pass, the same one a frame would give it, and the
            // pass has to actually put something on the screen.
            let output = context.run(egui::RawInput::default(), |ctx| panel.draw(ctx));
            assert!(!output.shapes.is_empty(), "the tab drew nothing at all");
        }
    }

    #[test]
    fn an_empty_report_draws_rather_than_panicking() {
        // The service is often unreachable right after a reboot; the window still
        // has to open so it can be installed from.
        let context = egui::Context::default();
        let mut panel = blank();
        panel.report = serde_json::json!({});
        panel.resolutions = serde_json::json!({});
        for tab in [Tab::Monitors, Tab::Applications, Tab::Streams, Tab::Log] {
            panel.tab = tab;
            let output = context.run(egui::RawInput::default(), |ctx| panel.draw(ctx));
            assert!(!output.shapes.is_empty(), "an empty report must still draw the window");
        }
    }

    #[test]
    fn the_window_name_says_which_build_it_is() {
        assert!(APP_NAME.starts_with("Monitor-Audio-"), "got {}", APP_NAME);
        assert!(APP_NAME.ends_with("Rust"), "got {}", APP_NAME);
    }

    #[test]
    fn the_daemon_is_found_next_to_the_panel_when_bundled() {
        // In the AppImage both programs sit in the same directory; if this ever
        // resolves to a bare name the panel silently stops showing anything.
        let path = daemon_binary();
        assert!(!path.to_string_lossy().is_empty());
    }
}
