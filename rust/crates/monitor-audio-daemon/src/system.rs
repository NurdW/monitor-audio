//! Everything here talks to the machine: helper commands, paths, moving streams.
//!
//! Split from the decision code so the decisions can be tested with a made-up
//! snapshot instead of needing a live PipeWire and a real stream to move.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use monitor_audio_core::config::{self, Config};
use monitor_audio_core::snapshot::{self, Sink, Stream};
use monitor_audio_core::Log;

fn env_dir(name: &str, fallback: &str) -> PathBuf {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(fallback),
    }
}

pub fn config_path() -> PathBuf {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => env_dir("HOME", "/tmp").join(".config"),
    };
    base.join("monitor-audio").join("config.json")
}

/// In the runtime directory so it disappears on its own and never outlives the
/// session.
pub fn silence_path() -> PathBuf {
    env_dir("XDG_RUNTIME_DIR", "/tmp").join("monitor-audio-silence.wav")
}

/// Run a helper command, returning its output, or `None` if it failed.
pub fn run(args: &[&str], log: Log) -> Option<String> {
    let output = match Command::new(args[0]).args(&args[1..]).output() {
        Ok(output) => output,
        Err(err) => {
            log(&format!("{} failed: {}", args[0], err));
            return None;
        }
    };
    if !output.status.success() {
        log(&format!(
            "{} exited {}: {}",
            args[0],
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim().chars().take(200).collect::<String>()
        ));
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Moving a stream is the one thing in here with an audible consequence, so it
/// sits behind a trait: the tests record what would have moved instead of
/// rearranging somebody's audio.
pub trait Mover {
    fn move_stream(&self, iid: &str, target: &str, log: Log) -> bool;
}

pub struct Pactl;

impl Mover for Pactl {
    fn move_stream(&self, iid: &str, target: &str, log: Log) -> bool {
        run(&["pactl", "move-sink-input", iid, target], log).is_some()
    }
}

/// One consistent read of settings, outputs, owners and streams, shared by one
/// pass. Re-reading everything each pass is what lets a display be unplugged or a
/// sound profile changed without restarting anything.
pub struct Snapshot {
    pub config: Config,
    pub sinks: Vec<Sink>,
    pub index_by_name: HashMap<String, String>,
    pub clients: HashMap<String, String>,
    pub streams: Vec<Stream>,
}

impl Snapshot {
    pub fn new(sinks: Vec<Sink>, clients: HashMap<String, String>, streams: Vec<Stream>,
               config: Config) -> Snapshot {
        let index_by_name = sinks.iter().map(|s| (s.name.clone(), s.index.clone())).collect();
        Snapshot { config, sinks, index_by_name, clients, streams }
    }

    pub fn gather(log: Log) -> Snapshot {
        let sinks = match run(&["pactl", "list", "sinks"], log) {
            Some(text) => snapshot::parse_sinks(&text),
            None => Vec::new(),
        };
        let clients = match run(&["pw-dump"], log) {
            Some(text) => snapshot::client_pids(&text, log),
            None => HashMap::new(),
        };
        let streams = match run(&["pactl", "list", "sink-inputs"], log) {
            Some(text) => snapshot::parse_sink_inputs(&text),
            None => Vec::new(),
        };
        let config = config::load_config(&config_path(), log);
        Snapshot::new(sinks, clients, streams, config)
    }

    pub fn index_of(&self, name: &str) -> Option<&str> {
        self.index_by_name.get(name).map(String::as_str)
    }
}

/// How long a gathered snapshot may be reused for a burst of Route calls.
///
/// The desktop integration reports every window when it loads, and each report
/// used to spawn pactl twice and pw-dump once: thirty windows meant ninety
/// subprocesses in a second, on a machine that is already busy logging in. A
/// short window collapses that without ever acting on data a polling pass has not
/// already seen. A move made inside the window is re-checked by the next pass.
pub const ROUTE_CACHE_TTL: Duration = Duration::from_millis(150);

static SNAPSHOT_CACHE: Mutex<Option<(Instant, Arc<Snapshot>)>> = Mutex::new(None);

/// A snapshot for a Route call, reused briefly while the desktop reports a burst
/// of windows. The polling pass uses `Snapshot::gather` and always sees fresh data.
pub fn gather_cached(log: Log) -> Arc<Snapshot> {
    let mut cache = match SNAPSHOT_CACHE.lock() {
        Ok(cache) => cache,
        Err(poisoned) => {
            // Another thread died mid-gather. The cache holds no invariant anyone
            // relies on, so take it back and carry on rather than stop the service.
            log("the snapshot cache lock was poisoned; recovering");
            poisoned.into_inner()
        }
    };
    if let Some((when, snapshot)) = cache.as_ref() {
        if when.elapsed() < ROUTE_CACHE_TTL {
            return snapshot.clone();
        }
    }
    let snapshot = Arc::new(Snapshot::gather(log));
    *cache = Some((Instant::now(), snapshot.clone()));
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_file_is_the_one_the_panel_writes() {
        // If these two ever disagree the panel appears to do nothing at all.
        let path = config_path();
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("config.json"));
        assert_eq!(path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()),
                   Some("monitor-audio"));
    }

    #[test]
    fn a_burst_of_route_calls_shares_one_snapshot() {
        // The point of the cache: the desktop reports every window at once and each
        // report would otherwise spawn three helper processes.
        let first = gather_cached(&|_: &str| {});
        let second = gather_cached(&|_: &str| {});
        assert!(Arc::ptr_eq(&first, &second), "the second call re-gathered instead of reusing");
    }

    #[test]
    fn the_silence_file_is_named_after_what_it_holds() {
        assert_eq!(silence_path().file_name().and_then(|n| n.to_str()),
                   Some("monitor-audio-silence.wav"));
    }
}
