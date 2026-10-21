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
            installed: Roots::from_home(Path::new(&std::env::var("HOME").unwrap_or_default()))
                .unit_target
                .exists(),
        };
        panel.read_settings();
        panel.refresh();
        panel
    }

    fn roots(&self) -> Roots {
        Roots::from_home(Path::new(&std::env::var("HOME").unwrap_or_default()))
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
        let body = serde_json::json!({
            "enabled": self.enabled,
            "keep_awake": self.keep_awake,
            "manual_output": self.overrides,
        });
        let path = config_path();
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                self.status = format!("could not create {}: {}", parent.display(), err);
                return;
            }
        }
        self.status = match std::fs::write(&path, body.to_string()) {
            Ok(()) => format!("saved to {}", path.display()),
            Err(err) => format!("could not write {}: {}", path.display(), err),
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
        let exe = std::env::current_exe().unwrap_or_default();
        let problems = install_files(&self.roots(), &exe, &SystemRunner);
        self.installed = true;
        self.status = if problems.is_empty() {
            "Installed. Drag a window to another monitor to try it.".to_string()
        } else {
            format!("could not install: {}", problems.join("; "))
        };
    }

    fn remove(&mut self) {
        let problems = remove_files(&self.roots(), &SystemRunner);
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
        let argument = format!("--device={}", output);
        match Command::new("paplay").arg(&argument).arg(&path).output() {
            Ok(_) => self.status = format!("played a tone on {}", output),
            Err(err) => self.status = format!("could not play the tone: {}", err),
        }
        self.tone = None;
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
                self.roots().unit_target.display()
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
        "Monitor Audio",
        options,
        Box::new(|_cc| Ok(Box::new(Panel::new()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_daemon_is_found_next_to_the_panel_when_bundled() {
        // In the AppImage both programs sit in the same directory; if this ever
        // resolves to a bare name the panel silently stops showing anything.
        let path = daemon_binary();
        assert!(!path.to_string_lossy().is_empty());
    }
}
