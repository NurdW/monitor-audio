//! The running state: which window wants which display's output, which streams
//! have already been placed, and which outputs are being held awake.
//!
//! Every method takes the snapshot it should reason from rather than gathering
//! one itself, so the whole of this can be tested against a made-up desktop.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use monitor_audio_core::routing::{self, Route};
use monitor_audio_core::snapshot::Stream;
use monitor_audio_core::{
    Log, BURST_SECONDS, KEEPALIVE_NAME, KEEPALIVE_RENEW_BEFORE, KEEPALIVE_SECONDS,
};

use crate::system::{run, silence_path, Mover, Snapshot};

/// What the desktop told us about one window.
#[derive(Clone, Debug, Default)]
pub struct RouteEntry {
    pub pid: String,
    pub model: String,
    /// The output this window was last sent to; a change here is the only thing
    /// allowed to move audio that has already been placed.
    pub output: Option<String>,
    pub appkeys: BTreeSet<String>,
    pub captions: BTreeSet<String>,
    pub warned: bool,
}

impl RouteEntry {
    fn as_route(&self) -> Route {
        Route {
            pid: self.pid.clone(),
            model: self.model.clone(),
            appkeys: self.appkeys.clone(),
            captions: self.captions.clone(),
        }
    }
}

struct KeepEntry {
    child: Child,
    started: Instant,
}

pub struct State {
    /// window id -> route. Keyed by window and not by process: one Firefox can
    /// have a window on the TV and a window on the desk, and their audio must not
    /// follow each other.
    pub wanted: HashMap<String, RouteEntry>,
    /// stream id -> window id that placed it. The point of this map is that a
    /// stream is placed once and then left alone until the next real drag.
    pub placed: HashMap<String, String>,
    keepalive: HashMap<String, KeepEntry>,
    /// An output whose silent stream would not start stays out of the way;
    /// retrying every pass would fill the log with the same failure.
    failed_keepalive: BTreeSet<String>,
    burst_until: Instant,
}

impl Default for State {
    fn default() -> Self {
        State {
            wanted: HashMap::new(),
            placed: HashMap::new(),
            keepalive: HashMap::new(),
            failed_keepalive: BTreeSet::new(),
            // A service that has just started is not in a burst; the first pass
            // waits the calm interval like any other.
            burst_until: Instant::now(),
        }
    }
}

/// Is this our own silent stream rather than somebody's audio?
///
/// `paplay` puts the `-n` name on application.name and node.name and the file
/// path in media.name, so both have to be checked. They are compared as written
/// and not folded: folding keeps only the last part of a dashed name, so our own
/// "monitor-audio-keepalive" arrives as "keepalive" and matched nothing, which
/// left the silent stream listed in the panel as if it were an app playing
/// nothing, and open to being routed like one.
fn is_keepalive(stream: &Stream, silence: &str) -> bool {
    stream.raw.iter().any(|value| value.as_str() == KEEPALIVE_NAME || value.as_str() == silence)
}

fn silence_name() -> String {
    silence_path().to_string_lossy().to_string()
}

fn process_alive(pid: &str) -> bool {
    // /proc is the portable-enough check here: this is Linux-only software, and a
    // missing directory means the process is gone for certain.
    pid.parse::<u32>().map(|pid| Path::new(&format!("/proc/{}", pid)).exists()).unwrap_or(false)
}

fn terminate(mut child: Child, log: Log) {
    // SIGTERM first, through `kill`, because sending one needs no libc binding
    // here; a silent stream that is asked politely releases its sink cleanly.
    run(&["kill", "-TERM", &child.id().to_string()], log);
    for _ in 0..30 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(err) => {
                log(&format!("could not wait for the silent stream to end: {}", err));
                break;
            }
        }
    }
    if let Err(err) = child.kill() {
        log(&format!("could not kill a silent stream that ignored SIGTERM: {}", err));
    }
    if let Err(err) = child.wait() {
        log(&format!("could not reap a silent stream: {}", err));
    }
}

impl State {
    /// Look again quickly for a while, because a stream is probably about to appear.
    pub fn start_burst(&mut self, reason: &str, log: Log) {
        let already = Instant::now() < self.burst_until;
        self.burst_until = Instant::now() + Duration::from_secs_f64(BURST_SECONDS);
        if !already {
            log(&format!("looking for new streams for {:.0} s ({})", BURST_SECONDS, reason));
        }
    }

    pub fn burst_is_running(&self) -> bool {
        Instant::now() < self.burst_until
    }

    /// Place this window's streams on the output of the display it sits on.
    ///
    /// `force` is set only by a real monitor change, and it is the one thing
    /// allowed to move a stream that has been placed before. A caption change,
    /// and every re-check, may only place streams nobody has ever placed.
    pub fn apply_window(
        &mut self,
        wid: &str,
        reason: &str,
        force: bool,
        snap: &Snapshot,
        mover: &dyn Mover,
        log: Log,
    ) -> usize {
        // Computed before the mutable borrow below, and from the map already held:
        // the old version cloned every route into a second map on every pass for
        // every window just to answer "is there another window with this pid".
        let pid = match self.wanted.get(wid) {
            Some(entry) => entry.pid.clone(),
            None => return 0,
        };
        let has_rival = self
            .wanted
            .iter()
            .any(|(other, other_entry)| other != wid && other_entry.pid == pid);
        let Some(entry) = self.wanted.get_mut(wid) else { return 0 };
        let (target, how) = routing::resolve_output(&entry.model, &snap.sinks, &snap.config, log);
        let Some(target) = target else {
            // Nothing to send audio to. Say so once, not every few seconds.
            if !entry.warned {
                entry.warned = true;
                let names: Vec<&str> = snap.sinks.iter().map(|s| s.name.as_str()).collect();
                log(&format!(
                    "display {:?} (window {}) has no usable output; outputs are: {}",
                    entry.model,
                    wid,
                    if names.is_empty() { "none".to_string() } else { names.join(", ") }
                ));
            }
            return 0;
        };
        entry.warned = false;
        if entry.output.as_deref() != Some(target.as_str()) {
            entry.output = Some(target.clone());
            log(&format!("display {:?} -> {} ({})", entry.model, target, how));
        }

        let route = entry.as_route();
        let target_index = snap.index_of(&target).map(str::to_string);
        let silence = silence_name();
        let mut moved = 0;
        for stream in &snap.streams {
            if is_keepalive(stream, &silence) {
                continue;
            }
            if !force && self.placed.contains_key(&stream.iid) {
                continue;
            }
            if !routing::stream_belongs(stream, &route, &snap.clients, has_rival) {
                continue;
            }
            if stream.sink == target_index {
                self.placed.insert(stream.iid.clone(), wid.to_string());
                continue;
            }
            if !mover.move_stream(&stream.iid, &target, log) {
                continue;
            }
            self.placed.insert(stream.iid.clone(), wid.to_string());
            moved += 1;
            log(&format!("stream {} (window {}) -> {} [{}]", stream.iid, wid, target, reason));
        }
        moved
    }

    /// Re-check every route, but only for streams never placed before.
    pub fn apply_pending(&mut self, snap: &Snapshot, mover: &dyn Mover, log: Log) -> usize {
        if self.wanted.is_empty() {
            return 0;
        }
        let live: BTreeSet<&str> = snap.streams.iter().map(|s| s.iid.as_str()).collect();
        let gone: Vec<String> = self
            .placed
            .keys()
            .filter(|iid| !live.contains(iid.as_str()))
            .cloned()
            .collect();
        for iid in &gone {
            self.placed.remove(iid);
        }
        // Placements are pruned even while routing is switched off, because
        // PipeWire reuses stream ids: a stale entry would make a brand-new stream
        // look already placed and leave it on the wrong output after switching
        // routing back on.
        if !snap.config.enabled {
            return 0;
        }
        if !gone.is_empty() {
            // A player that drops its stream while paused usually makes a new one
            // the instant it resumes; that new stream is what needs catching fast.
            self.start_burst(&format!("{} placed stream(s) disappeared", gone.len()), log);
        }
        let mut moved = 0;
        for wid in self.wanted.keys().cloned().collect::<Vec<String>>() {
            moved += self.apply_window(&wid, "new stream", false, snap, mover, log);
        }
        moved
    }

    /// Forget windows whose process is gone; KWin reports closes, this is the net.
    pub fn prune_dead(&mut self, log: Log) {
        let dead: Vec<String> = self
            .wanted
            .iter()
            .filter(|(_, entry)| !process_alive(&entry.pid))
            .map(|(wid, _)| wid.clone())
            .collect();
        for wid in dead {
            let pid = self.wanted[&wid].pid.clone();
            log(&format!("forgetting window {} (pid {} is not running)", wid, pid));
            self.wanted.remove(&wid);
        }
    }

    pub fn describe_streams(&self, snap: &Snapshot) -> Vec<String> {
        let silence = silence_name();
        snap.streams
            .iter()
            .filter(|stream| !is_keepalive(stream, &silence))
            .map(|stream| {
                let owners = routing::owner_pids(stream, &snap.clients);
                let placed_by = self
                    .placed
                    .get(&stream.iid)
                    .map(|wid| format!("  (placed by {})", wid))
                    .unwrap_or_default();
                format!(
                    "  stream {} sink={} pids={} apps={} titles={}{}",
                    stream.iid,
                    stream.sink.clone().unwrap_or_else(|| "?".into()),
                    joined(&owners, "none"),
                    joined(&stream.appkeys, "none"),
                    joined(&stream.titles, "none"),
                    placed_by
                )
            })
            .collect()
    }

    /// Everything the control panel shows, as plain data.
    pub fn report(&self, snap: &Snapshot, service_running: bool) -> serde_json::Value {
        let name_by_index: HashMap<&str, &str> =
            snap.sinks.iter().map(|s| (s.index.as_str(), s.name.as_str())).collect();

        let mut windows: Vec<&String> = self.wanted.keys().collect();
        windows.sort();
        let routes: Vec<serde_json::Value> = windows
            .iter()
            .map(|wid| {
                let entry = &self.wanted[*wid];
                let (output, how) = routing::resolve_output(&entry.model, &snap.sinks, &snap.config, &silent);
                serde_json::json!({
                    "window": wid,
                    "pid": entry.pid,
                    "model": entry.model,
                    "output": output,
                    "how": how,
                    "apps": entry.appkeys.iter().cloned().collect::<Vec<String>>(),
                    "captions": entry.captions.iter().cloned().collect::<Vec<String>>(),
                })
            })
            .collect();

        let silence = silence_name();
        let streams: Vec<serde_json::Value> = snap
            .streams
            .iter()
            .filter(|stream| !is_keepalive(stream, &silence))
            .map(|stream| {
                serde_json::json!({
                    "id": stream.iid,
                    "output": stream.sink.as_deref().and_then(|index| name_by_index.get(index)).copied(),
                    "apps": stream.appkeys.iter().cloned().collect::<Vec<String>>(),
                    "titles": stream.titles.iter().cloned().collect::<Vec<String>>(),
                    "pids": routing::owner_pids(stream, &snap.clients).into_iter().collect::<Vec<String>>(),
                    "placed_by": self.placed.get(&stream.iid),
                })
            })
            .collect();

        let manual: serde_json::Map<String, serde_json::Value> = snap
            .config
            .manual_output
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();

        serde_json::json!({
            "service_running": service_running,
            "config": {
                "enabled": snap.config.enabled,
                "keep_awake": snap.config.keep_awake,
                "fallback_kind": snap.config.fallback_kind,
                "manual_output": manual,
            },
            "config_path": crate::system::config_path().to_string_lossy(),
            "outputs": snap.sinks.iter().map(|s| serde_json::json!({
                "name": s.name, "description": s.description, "index": s.index })).collect::<Vec<_>>(),
            "routes": routes,
            "streams": streams,
        })
    }

    pub fn status_text(&self, snap: &Snapshot) -> String {
        let mut lines = vec![
            format!("settings ({}):", crate::system::config_path().display()),
            format!(
                "  enabled={} fallback_kind={:?} manual_output={:?}",
                snap.config.enabled, snap.config.fallback_kind, snap.config.manual_output
            ),
            "outputs present:".to_string(),
        ];
        for sink in &snap.sinks {
            lines.push(format!("  {}  ({})", sink.name, sink.description));
        }
        if self.wanted.is_empty() {
            lines.push("no routes remembered yet (drag a window to another monitor)".to_string());
        }
        let mut windows: Vec<&String> = self.wanted.keys().collect();
        windows.sort();
        for wid in windows {
            let entry = &self.wanted[wid];
            let (target, how) =
                routing::resolve_output(&entry.model, &snap.sinks, &snap.config, &silent);
            lines.push(format!(
                "window {} (pid {}) display {:?} -> {:?} [{}] (names: {}; captions: {})",
                wid,
                entry.pid,
                entry.model,
                target,
                how,
                joined(&entry.appkeys, "?"),
                joined(&entry.captions, "?")
            ));
        }
        lines.push(format!("placed streams: {}", self.placed.len()));
        lines.push("live streams:".to_string());
        let described = self.describe_streams(snap);
        if described.is_empty() {
            lines.push("  (nothing is playing)".to_string());
        } else {
            lines.extend(described);
        }
        lines.join("\n")
    }

    /// Forget every route and placement, e.g. after rewiring the speakers.
    pub fn reset(&mut self, log: Log) -> String {
        let count = self.wanted.len();
        self.wanted.clear();
        self.placed.clear();
        log(&format!("reset: forgot {} route(s)", count));
        format!("forgot {} route(s)", count)
    }

    /// Drop one window's route, or every route of a process when given a pid.
    pub fn forget(&mut self, key: &str, log: Log) -> String {
        let dropped: Vec<String> = if self.wanted.contains_key(key) {
            vec![key.to_string()]
        } else {
            self.wanted
                .iter()
                .filter(|(_, entry)| entry.pid == key)
                .map(|(wid, _)| wid.clone())
                .collect()
        };
        for wid in &dropped {
            self.wanted.remove(wid);
        }
        let streams: Vec<String> = self
            .placed
            .iter()
            .filter(|(_, wid)| dropped.contains(wid))
            .map(|(iid, _)| iid.clone())
            .collect();
        for iid in streams {
            self.placed.remove(&iid);
        }
        if dropped.is_empty() {
            return format!("nothing remembered for {}", key);
        }
        log(&format!("forgot {} route(s) for {}", dropped.len(), key));
        format!("forgot {} route(s) for {}", dropped.len(), key)
    }

    pub fn stop_keepalive(&mut self, reason: &str, log: Log) {
        if !self.keepalive.is_empty() && !reason.is_empty() {
            log(&format!(
                "stopped holding {} output(s) awake ({})",
                self.keepalive.len(),
                reason
            ));
        }
        let entries: Vec<KeepEntry> = self.keepalive.drain().map(|(_, entry)| entry).collect();
        for entry in entries {
            terminate(entry.child, log);
        }
    }

    fn start_keepalive(&mut self, name: &str, log: Log) -> bool {
        let silence = silence_path();
        let child = Command::new("paplay")
            .arg("-n")
            .arg(KEEPALIVE_NAME)
            .arg(format!("--device={}", name))
            .arg(&silence)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => {
                self.keepalive.insert(name.to_string(), KeepEntry { child, started: Instant::now() });
                true
            }
            // Most likely paplay is missing on this machine; holding outputs awake
            // is a nicety, so say so once per output and carry on routing.
            Err(err) => {
                log(&format!("could not start the silent stream to hold {} awake: {}", name, err));
                self.keepalive.remove(name);
                self.failed_keepalive.insert(name.to_string());
                false
            }
        }
    }

    /// Write the silent file the keep-alive streams read. True when it is usable.
    ///
    /// A file left over from an older version plays for less time than the service
    /// now expects, and every renewal would then read as a stream dying early.
    pub fn ensure_silence(&self, log: Log) -> bool {
        let path = silence_path();
        let needed = 44 + 8000 * KEEPALIVE_SECONDS;
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() >= needed {
                return true;
            }
        }
        match write_silence(&path, KEEPALIVE_SECONDS as usize) {
            Ok(()) => true,
            Err(err) => {
                log(&format!("could not write {} to keep outputs awake: {}", path.display(), err));
                false
            }
        }
    }

    /// Hold one silent stream on every output a route points at.
    ///
    /// Without this the output suspends after five idle seconds and the first
    /// seconds of whatever follows are lost while it wakes up. The stream is
    /// visible in the volume mixer, so settings can switch the whole thing off.
    pub fn keepalive_tick(&mut self, snap: &Snapshot, log: Log) {
        if self.wanted.is_empty() {
            self.stop_keepalive("", log);
            return;
        }
        if !snap.config.enabled || !snap.config.keep_awake {
            self.stop_keepalive("switched off", log);
            return;
        }
        if !self.ensure_silence(log) {
            return;
        }

        // Hold one silent stream on every output a route points at, whether or not
        // something is playing there. Deciding "is anything playing?" cannot be
        // done from pactl, which leaves media.name off Pulse clients - our own
        // stream would look like a stranger's, and stopping and restarting it
        // every pass was worse than leaving it up.
        let mut targets: BTreeSet<String> = BTreeSet::new();
        for entry in self.wanted.values() {
            if let (Some(target), _) = routing::resolve_output(&entry.model, &snap.sinks, &snap.config, log) {
                targets.insert(target);
            }
        }
        targets = targets.difference(&self.failed_keepalive).cloned().collect();

        let stale: Vec<String> = self
            .keepalive
            .keys()
            .filter(|name| !targets.contains(*name))
            .cloned()
            .collect();
        for name in stale {
            if let Some(entry) = self.keepalive.remove(&name) {
                terminate(entry.child, log);
            }
        }

        for name in targets {
            let existing = self.keepalive.get_mut(&name).map(|entry| {
            (matches!(entry.child.try_wait(), Ok(None)), entry.started.elapsed())
        });
        if let Some((running, elapsed)) = existing {
            let due = elapsed >= Duration::from_secs(KEEPALIVE_SECONDS - KEEPALIVE_RENEW_BEFORE);
            if running && !due {
                continue;
            }
            log(&format!(
                "silent stream on {} {}; starting another",
                name,
                if running { "is about to run out" } else { "ended early" }
            ));
            // The replacement goes up before the old one comes down. A gap of even
            // a few seconds is enough for the output to start suspending again,
            // which is the very thing this exists to prevent; two silent streams
            // on one output for a moment costs nothing.
            if let Some(entry) = self.keepalive.remove(&name) {
                terminate(entry.child, log);
            }
        }
            if self.start_keepalive(&name, log) {
                log(&format!("holding {} awake with a silent stream", name));
            }
        }
    }
}

/// For places that resolve an output to report on it: nothing the user should be
/// told about, and nothing to swallow either.
fn silent(_msg: &str) {}


/// The strings KWin sends with a window, kept together: the D-Bus signature is
/// fixed at seven strings by `callDBus`, but the handler has no reason to take
/// them one at a time.
#[derive(Clone, Debug, Default)]
pub struct RouteRequest {
    pub pid: String,
    pub resource_class: String,
    pub desktop_file: String,
    pub caption: String,
    pub window: String,
    pub model: String,
}

impl State {
    /// Record what the desktop just told us about a window, and move its audio if
    /// that means a different output.
    pub fn handle_route(
        &mut self,
        request: &RouteRequest,
        snap: &Snapshot,
        mover: &dyn Mover,
        log: Log,
    ) -> String {
        let pid = request.pid.trim();
        let window = if request.window.trim().is_empty() { pid.to_string() } else { request.window.trim().to_string() };
        if pid.parse::<u32>().map(|value| value == 0).unwrap_or(true) {
            // KWin reports -1 when it cannot resolve a window's owner. Remembering
            // such a route would let a phantom window claim streams by name.
            log(&format!("Route ignored for window {}: unusable pid {:?}", window, pid));
            return format!("ignoring window {} with unusable pid", window);
        }

        let mut appkeys: BTreeSet<String> = [
            routing::norm(&request.resource_class),
            routing::norm(
                std::path::Path::new(&request.desktop_file)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .as_str(),
            ),
        ]
        .into_iter()
        .collect();
        match std::fs::read_to_string(format!("/proc/{}/comm", pid)) {
            Ok(comm) => {
                appkeys.insert(routing::norm(&comm));
            }
            Err(err) => log(&format!(
                "could not read the process name of pid {} (window {}): {}",
                pid, window, err
            )),
        }
        appkeys.remove("");
        let mut captions: BTreeSet<String> = BTreeSet::new();
        let folded = routing::norm(&request.caption);
        if !folded.is_empty() {
            captions.insert(folded);
        }
        let model = request.model.trim().to_string();

        let old = self.wanted.get(&window).cloned();
        let moved_monitor = old.as_ref().map(|old| old.model != model).unwrap_or(false);
        if let Some(old) = &old {
            if old.model == model && old.captions == captions {
                return format!(
                    "unchanged: window {} stays on {}",
                    window,
                    if model.is_empty() { "unknown display" } else { &model }
                );
            }
        }
        self.wanted.insert(
            window.clone(),
            RouteEntry {
                pid: pid.to_string(),
                model: model.clone(),
                output: old.as_ref().and_then(|old| old.output.clone()),
                appkeys,
                captions,
                warned: old.as_ref().map(|old| old.warned).unwrap_or(false),
            },
        );
        match &old {
            None => log(&format!(
                "Route: window {} pid {} display {} class={} caption={}",
                window,
                pid,
                model,
                request.resource_class,
                request.caption.chars().take(70).collect::<String>()
            )),
            Some(_) if moved_monitor => log(&format!(
                "Route: window {} moved to display {} caption={}",
                window,
                model,
                request.caption.chars().take(70).collect::<String>()
            )),
            Some(_) => {}
        }

        if !snap.config.enabled {
            // The window is still remembered. A window that arrived while routing
            // was switched off used to stay unknown after switching it back on,
            // silent until its title changed or it crossed onto another display.
            return format!("remembered window {}; routing is switched off", window);
        }

        // A window arriving, or crossing onto another display, is followed shortly
        // by that app creating a stream. A caption change alone is not worth it:
        // browsers rewrite titles constantly and polling fast after each one would
        // keep a core busy while browsing.
        if old.is_none() || moved_monitor {
            self.start_burst(
                &format!("window {} on {}", window, if model.is_empty() { "unknown display" } else { &model }),
                log,
            );
        }

        // Only a display change is allowed to move audio that is already placed.
        let moved = self.apply_window(&window, "window moved", moved_monitor, snap, mover, log);
        if moved == 0 && moved_monitor {
            let detail = self.describe_streams(snap).join("\n");
            return format!(
                "window {} -> display {}, but no stream was moved; streams now:\n{}",
                window,
                model,
                if detail.is_empty() { "  (nothing is playing)".to_string() } else { detail }
            );
        }
        format!("window {} on {} ({} stream(s) moved)", window, model, moved)
    }
}

fn joined(values: &BTreeSet<String>, if_empty: &str) -> String {
    if values.is_empty() {
        if_empty.to_string()
    } else {
        values.iter().cloned().collect::<Vec<String>>().join(", ")
    }
}

/// A wav of digital silence: 8 kHz, mono, unsigned 8-bit, where silence is 0x80
/// and not zero.
fn write_silence(path: &Path, seconds: usize) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data_len = 8000 * seconds;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    write_u32(&mut header, 4, 36 + data_len as u32);
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    write_u32(&mut header, 16, 16);
    header[20] = 1; // PCM
    write_u16(&mut header, 22, 1); // channels
    write_u32(&mut header, 24, 8000); // rate
    write_u32(&mut header, 28, 8000); // byte rate
    write_u16(&mut header, 32, 1); // block align
    write_u16(&mut header, 34, 8); // bits
    header[36..40].copy_from_slice(b"data");
    write_u32(&mut header, 40, data_len as u32);
    file.write_all(&header)?;
    for _ in 0..seconds {
        file.write_all(&[0x80u8; 8000])?;
    }
    file.flush()
}

fn write_u16(buffer: &mut [u8], at: usize, value: u16) {
    buffer[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(buffer: &mut [u8], at: usize, value: u32) {
    buffer[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::config_path;
    use monitor_audio_core::config::Config;
    use monitor_audio_core::snapshot::Sink;
    use std::cell::RefCell;

    fn quiet(_msg: &str) {}

    #[derive(Default)]
    struct Recorder {
        moves: RefCell<Vec<(String, String)>>,
    }

    impl Mover for Recorder {
        fn move_stream(&self, iid: &str, target: &str, _log: Log) -> bool {
            self.moves.borrow_mut().push((iid.to_string(), target.to_string()));
            true
        }
    }

    fn sink(name: &str, index: &str, description: &str) -> Sink {
        Sink { name: name.to_string(), description: description.to_string(), index: index.to_string() }
    }

    fn snapshot(streams: Vec<Stream>) -> Snapshot {
        let sinks = vec![
            sink("alsa.analog", "47", "Starship/Matisse Analog Stereo"),
            sink("alsa.hdmi", "48", "Navi 21 HDMI (HDMI) [SAMSUNG]"),
        ];
        Snapshot::new(sinks, HashMap::new(), streams, Config::default())
    }

    fn stream(iid: &str, sink_index: &str, pid: &str) -> Stream {
        Stream {
            iid: iid.to_string(),
            sink: Some(sink_index.to_string()),
            pids: [pid.to_string()].into_iter().collect(),
            ..Default::default()
        }
    }

    fn route_entry(pid: &str, model: &str) -> RouteEntry {
        RouteEntry { pid: pid.to_string(), model: model.to_string(), ..Default::default() }
    }

    fn state_with(wid: &str, entry: RouteEntry) -> State {
        let mut state = State::default();
        state.wanted.insert(wid.to_string(), entry);
        state
    }

    #[test]
    fn a_window_on_the_tv_moves_its_stream_to_the_tv() {
        let mut state = state_with("w1", route_entry("100", "SAMSUNG"));
        let snap = snapshot(vec![stream("5", "47", "100")]);
        let recorder = Recorder::default();
        assert_eq!(state.apply_window("w1", "test", false, &snap, &recorder, &quiet), 1);
        assert_eq!(recorder.moves.borrow().as_slice(),
                   &[("5".to_string(), "alsa.hdmi".to_string())]);
        assert_eq!(state.placed.get("5").map(String::as_str), Some("w1"));
    }

    #[test]
    fn a_stream_is_never_moved_twice_without_a_real_monitor_change() {
        let mut state = state_with("w1", route_entry("100", "SAMSUNG"));
        let snap = snapshot(vec![stream("5", "47", "100")]);
        let recorder = Recorder::default();
        state.apply_window("w1", "first", false, &snap, &recorder, &quiet);
        state.apply_window("w1", "second", false, &snap, &recorder, &quiet);
        assert_eq!(recorder.moves.borrow().len(), 1, "the second pass must not fight the first");
    }

    #[test]
    fn a_real_monitor_change_may_move_what_was_already_placed() {
        let mut state = state_with("w1", route_entry("100", "SAMSUNG"));
        let recorder = Recorder::default();
        state.apply_window("w1", "first", false, &snapshot(vec![stream("5", "47", "100")]), &recorder, &quiet);
        // The first move worked, so the stream now really is on the TV's output;
        // a snapshot that still showed it on the desk would hide the second move.
        state.wanted.get_mut("w1").unwrap().model = "2778X".to_string();
        assert_eq!(state.apply_window("w1", "moved", true, &snapshot(vec![stream("5", "48", "100")]), &recorder, &quiet), 1);
        assert_eq!(recorder.moves.borrow().len(), 2);
        assert_eq!(recorder.moves.borrow()[1].1, "alsa.analog");
    }

    #[test]
    fn a_stream_already_where_it_belongs_is_recorded_without_being_moved() {
        let mut state = state_with("w1", route_entry("100", "SAMSUNG"));
        let snap = snapshot(vec![stream("5", "48", "100")]);
        let recorder = Recorder::default();
        state.apply_window("w1", "test", false, &snap, &recorder, &quiet);
        assert!(recorder.moves.borrow().is_empty());
        assert!(state.placed.contains_key("5"));
    }

    #[test]
    fn our_own_silent_stream_is_never_routed() {
        let mut silent = stream("9", "47", "100");
        silent.raw.insert(monitor_audio_core::KEEPALIVE_NAME.to_string());
        let mut state = state_with("w1", route_entry("100", "SAMSUNG"));
        let snap = snapshot(vec![silent]);
        let recorder = Recorder::default();
        state.apply_window("w1", "test", false, &snap, &recorder, &quiet);
        assert!(recorder.moves.borrow().is_empty(), "moving our own silence would fight itself");
    }

    #[test]
    fn a_window_with_no_output_warns_once_and_moves_nothing() {
        let mut state = state_with("w1", route_entry("100", "Unknown 9"));
        let mut snap = snapshot(vec![stream("5", "47", "100")]);
        snap.config.fallback_kind = "nothing-matches-this".to_string();
        let recorder = Recorder::default();
        let messages = std::cell::RefCell::new(Vec::new());
        let collect = |m: &str| messages.borrow_mut().push(m.to_string());
        state.apply_window("w1", "test", false, &snap, &recorder, &collect);
        state.apply_window("w1", "test", false, &snap, &recorder, &collect);
        let messages = messages.borrow();
        assert_eq!(messages.len(), 1, "the same warning every few seconds is noise: {:?}", messages);
        assert!(recorder.moves.borrow().is_empty());
    }

    #[test]
    fn placements_for_streams_that_went_away_are_pruned_even_when_switched_off() {
        let mut state = state_with("w1", route_entry("100", "SAMSUNG"));
        state.placed.insert("5".to_string(), "w1".to_string());
        let mut snap = snapshot(vec![]);
        snap.config.enabled = false;
        state.apply_pending(&snap, &Recorder::default(), &quiet);
        assert!(state.placed.is_empty(),
                "PipeWire reuses stream ids, so a stale placement would hide a new stream");
    }

    #[test]
    fn forgetting_a_window_drops_its_placements_too() {
        let mut state = state_with("w1", route_entry("100", "SAMSUNG"));
        state.placed.insert("5".to_string(), "w1".to_string());
        state.forget("w1", &quiet);
        assert!(state.wanted.is_empty() && state.placed.is_empty());
    }

    #[test]
    fn forgetting_a_pid_forgets_every_window_of_that_process() {
        let mut state = State::default();
        state.wanted.insert("w1".into(), route_entry("100", "SAMSUNG"));
        state.wanted.insert("w2".into(), route_entry("100", "2778X"));
        state.forget("100", &quiet);
        assert!(state.wanted.is_empty());
    }

    #[test]
    fn a_window_whose_process_has_gone_is_forgotten() {
        let state = state_with("w1", route_entry("999999", "SAMSUNG"));
        let mut state = state;
        state.prune_dead(&quiet);
        assert!(state.wanted.is_empty(), "a dead pid must not keep claiming streams");
    }

    #[test]
    fn the_report_hides_our_own_silent_stream_but_shows_the_others() {
        let mut silent = stream("9", "47", "100");
        silent.raw.insert(monitor_audio_core::KEEPALIVE_NAME.to_string());
        let state = state_with("w1", route_entry("100", "SAMSUNG"));
        let snap = snapshot(vec![silent, stream("5", "47", "100")]);
        let report = state.report(&snap, true);
        let ids: Vec<&str> = report["streams"].as_array().unwrap()
            .iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["5"], "the mixer already shows enough of our housekeeping");
        assert_eq!(report["routes"][0]["output"].as_str(), Some("alsa.hdmi"));
    }

    #[test]
    fn a_window_with_no_resolvable_process_is_not_remembered() {
        // KWin reports -1 when it cannot resolve an owner; remembering it would let
        // a phantom window claim streams by application name.
        let mut state = State::default();
        let request = RouteRequest { pid: "-1".into(), window: "w1".into(), model: "SAMSUNG".into(), ..Default::default() };
        let snap = snapshot(vec![]);
        let answer = state.handle_route(&request, &snap, &Recorder::default(), &quiet);
        assert!(answer.contains("unusable pid"), "got: {}", answer);
        assert!(state.wanted.is_empty());
    }

    #[test]
    fn the_same_report_from_the_desktop_is_not_reapplied() {
        let pid = std::process::id().to_string();
        let mut state = State::default();
        let request = RouteRequest {
            pid: pid.clone(),
            window: "w1".into(),
            model: "SAMSUNG".into(),
            caption: "Stranger Things".into(),
            ..Default::default()
        };
        let snap = snapshot(vec![stream("5", "47", &pid)]);
        state.handle_route(&request, &snap, &Recorder::default(), &quiet);
        let second = state.handle_route(&request, &snap, &Recorder::default(), &quiet);
        assert!(second.starts_with("unchanged"), "got: {}", second);
    }

    #[test]
    fn a_window_that_arrives_while_switched_off_is_still_remembered() {
        // Otherwise switching routing back on leaves the window unknown until its
        // title changes or it crosses onto another display.
        let pid = std::process::id().to_string();
        let mut state = State::default();
        let mut snap = snapshot(vec![stream("5", "47", &pid)]);
        snap.config.enabled = false;
        let request = RouteRequest { pid, window: "w1".into(), model: "SAMSUNG".into(), ..Default::default() };
        let answer = state.handle_route(&request, &snap, &Recorder::default(), &quiet);
        assert!(answer.contains("routing is switched off"), "got: {}", answer);
        assert!(state.wanted.contains_key("w1"));
    }

    #[test]
    fn a_new_window_starts_a_burst_because_its_stream_is_coming() {
        let pid = std::process::id().to_string();
        let mut state = State::default();
        let snap = snapshot(vec![stream("5", "47", &pid)]);
        let request = RouteRequest { pid, window: "w1".into(), model: "SAMSUNG".into(), ..Default::default() };
        state.handle_route(&request, &snap, &Recorder::default(), &quiet);
        assert!(state.burst_is_running(), "the stream arrives a moment after the window does");
    }


    fn titled(iid: &str, sink_index: &str, pid: &str, title: &str) -> Stream {
        let mut stream = stream(iid, sink_index, pid);
        stream.titles.insert(title.to_string());
        stream
    }

    fn captioned(pid: &str, model: &str, caption: &str) -> RouteEntry {
        let mut entry = route_entry(pid, model);
        entry.captions.insert(caption.to_string());
        entry
    }

    #[test]
    fn one_browser_window_on_the_tv_only_moves_the_stream_whose_title_matches() {
        // Two windows of one Firefox process on two displays: the caption is the
        // only thing that says which tab's audio belongs to which window.
        let mut state = State::default();
        state.wanted.insert("tv".into(), captioned("100", "SAMSUNG", "stranger things"));
        state.wanted.insert("desk".into(), captioned("100", "2778X", "reddit"));
        let snap = snapshot(vec![
            titled("5", "47", "100", "stranger things"),
            titled("6", "47", "100", "reddit"),
        ]);
        let recorder = Recorder::default();
        state.apply_window("tv", "test", false, &snap, &recorder, &quiet);
        let moved: Vec<String> = recorder.moves.borrow().iter().map(|(iid, _)| iid.clone()).collect();
        assert_eq!(moved, vec!["5".to_string()], "only the TV tab's stream may move");
    }

    #[test]
    fn a_titled_stream_from_a_process_with_one_known_window_is_claimed() {
        // The counterpart of the rule above, and the reason it is safe: with every
        // window registered by the desktop integration, no rival genuinely means no
        // other window, so an ordinary single-window player is never left silent
        // even when its stream title has nothing in common with its caption.
        let mut state = State::default();
        state.wanted.insert("tv".into(), captioned("100", "SAMSUNG", "stranger things"));
        let snap = snapshot(vec![titled("5", "47", "100", "stranger things")]);
        let recorder = Recorder::default();
        state.apply_window("tv", "test", false, &snap, &recorder, &quiet);
        assert_eq!(recorder.moves.borrow().len(), 1);
    }

    #[test]
    fn a_single_window_app_with_an_unrelated_stream_title_still_gets_its_audio() {
        // The other rule, pulling the other way: a lone player whose stream title
        // has nothing to do with its window caption must still be routed, or the
        // guard above would silence ordinary players.
        let mut state = State::default();
        state.wanted.insert("tv".into(), captioned("200", "SAMSUNG", "some video - mpv"));
        let snap = snapshot(vec![stream("7", "47", "200")]);
        let recorder = Recorder::default();
        state.apply_window("tv", "test", false, &snap, &recorder, &quiet);
        assert_eq!(recorder.moves.borrow().len(), 1, "a lone window's stream has no rival to fear");
    }

    #[test]
    fn settings_are_read_from_the_path_the_panel_writes() {
        // If these two ever disagree the panel appears to do nothing at all.
        assert_eq!(config_path().file_name().unwrap().to_str(), Some("config.json"));
        assert_eq!(config_path().parent().unwrap().file_name().unwrap().to_str(), Some("monitor-audio"));
    }
}
