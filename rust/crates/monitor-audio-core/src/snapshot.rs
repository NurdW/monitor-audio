//! Reading what PipeWire and its PulseAudio compatibility layer currently report.
//!
//! Parsing is done here rather than through a libpulse binding so the Rust port
//! observes exactly what the Python version observed: `pactl` cannot see client
//! pids and `pw-dump` cannot see sink indexes, so both are needed and both are
//! text.

use std::collections::{BTreeSet, HashMap};

use regex::Regex;

use crate::routing::norm;
use crate::Log;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sink {
    pub name: String,
    pub description: String,
    /// A stream reports the output it is on by index while everything else here
    /// identifies outputs by name, so the index has to be carried along.
    pub index: String,
}

#[derive(Clone, Debug, Default)]
pub struct Stream {
    pub iid: String,
    pub sink: Option<String>,
    pub client: Option<String>,
    pub pids: BTreeSet<String>,
    pub appkeys: BTreeSet<String>,
    pub titles: BTreeSet<String>,
    /// The same application and title values as written, not folded. Telling our
    /// own silent stream apart from a stranger's needs the exact text: folding
    /// "monitor-audio-keepalive" leaves "keepalive", which matches nothing.
    pub raw: BTreeSet<String>,
}

/// `[name, description]` pairs, in the order pactl listed them: the first match
/// wins in several places, so order is part of the contract.
pub fn parse_sinks(text: &str) -> Vec<Sink> {
    let header = Regex::new(r"^Sink #(\d+)").expect("valid sink header regex");
    let name_line = Regex::new(r"^\tName: (\S+)").expect("valid sink name regex");
    let desc_line = Regex::new(r"^\tDescription: (.+)$").expect("valid sink description regex");

    let mut sinks: Vec<Sink> = Vec::new();
    for line in text.lines() {
        if let Some(captures) = header.captures(line) {
            sinks.push(Sink {
                name: String::new(),
                description: String::new(),
                index: captures[1].to_string(),
            });
            continue;
        }
        if let Some(last) = sinks.last_mut() {
            if let Some(captures) = name_line.captures(line) {
                last.name = captures[1].to_string();
            } else if let Some(captures) = desc_line.captures(line) {
                last.description = captures[1].trim().to_string();
            }
        }
    }
    sinks.retain(|sink| !sink.name.is_empty());
    sinks
}

/// One entry per playing stream: what to move, where it is, who owns it.
pub fn parse_sink_inputs(text: &str) -> Vec<Stream> {
    let prop = Regex::new(r#"\s*([a-zA-Z0-9._-]+)\s*=\s*"([^"]*)"\s*$"#)
        .expect("valid property regex");
    let sink_line = Regex::new(r"\s*Sink:\s*(\d+)").expect("valid sink assignment regex");

    let mut streams: Vec<Stream> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Sink Input #") {
            streams.push(Stream {
                iid: rest.split_whitespace().next().unwrap_or("").to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(last) = streams.last_mut() else { continue };
        if let Some(captures) = sink_line.captures(line) {
            last.sink = Some(captures[1].to_string());
            continue;
        }
        let Some(captures) = prop.captures(line) else { continue };
        let (key, value) = (&captures[1], captures[2].to_string());
        match key {
            "application.process.id" => {
                last.pids.insert(value);
            }
            "client.id" => {
                last.client = Some(value);
            }
            // Several properties name the same application in different ways; all
            // of them are kept because different clients set different subsets.
            "application.name" | "application.id" | "application.process.binary" | "node.name" => {
                last.appkeys.insert(norm(&value));
                last.raw.insert(value);
            }
            "media.name" => {
                last.titles.insert(norm(&value));
                last.raw.insert(value);
            }
            _ => {}
        }
    }
    streams
}

/// {client node id: process id} straight from PipeWire.
///
/// `pactl` never reports client pids and natively-connected apps put their pid
/// only on the client node. Pids belonging to PipeWire itself are dropped: every
/// PulseAudio-compatibility stream is owned by that daemon, so keeping it would
/// make unrelated streams look related.
pub fn client_pids(dump_json: &str, log: Log) -> HashMap<String, String> {
    let data: serde_json::Value = match serde_json::from_str(dump_json) {
        Ok(data) => data,
        Err(err) => {
            log(&format!("pw-dump returned unreadable json: {}", err));
            return HashMap::new();
        }
    };
    let objects = match data.as_array() {
        Some(objects) => objects.clone(),
        None => {
            log("pw-dump returned json that is not a list; ignoring it");
            return HashMap::new();
        }
    };

    let daemon_names = ["pipewire", "pipewire-pulse", "wireplumber"];
    let mut daemon_pids: BTreeSet<String> = BTreeSet::new();
    for object in &objects {
        let props = &object["info"]["props"];
        if daemon_names.iter().any(|name| props["application.name"].as_str() == Some(name)) {
            if let Some(pid) = props["pipewire.sec.pid"].as_i64() {
                daemon_pids.insert(pid.to_string());
            }
        }
    }

    let mut pids = HashMap::new();
    for object in &objects {
        if object["type"].as_str() != Some("PipeWire:Interface:Client") {
            continue;
        }
        let props = &object["info"]["props"];
        if let Some(pid) = props["pipewire.sec.pid"].as_i64() {
            let id = object["id"].as_i64().map(|id| id.to_string());
            if pid > 0 && !daemon_pids.contains(&pid.to_string()) {
                if let Some(id) = id {
                    pids.insert(id, pid.to_string());
                }
            }
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(_msg: &str) {}

    const SINKS: &str = "\
Sink #47
	State: RUNNING
	Name: alsa_output.pci-0000_09_00.4.analog-stereo
	Description: Starship/Matisse HD Audio Controller Analog Stereo
	Volume: mono: 65536
Sink #48
	State: SUSPENDED
	Name: alsa_output.pci-0000_07_00.1.hdmi-stereo
	Description: Navi 21/23 HDMI/DP Audio Controller Digital Stereo (HDMI) [SAMSUNG]
";

    #[test]
    fn sinks_carry_name_description_and_index() {
        let sinks = parse_sinks(SINKS);
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].index, "47");
        assert_eq!(sinks[0].name, "alsa_output.pci-0000_09_00.4.analog-stereo");
        assert_eq!(sinks[1].description,
                   "Navi 21/23 HDMI/DP Audio Controller Digital Stereo (HDMI) [SAMSUNG]");
    }

    #[test]
    fn a_sink_without_a_name_is_not_offered() {
        let sinks = parse_sinks("Sink #1\n\tDescription: nameless\n");
        assert!(sinks.is_empty(), "a sink nothing can address must not be listed");
    }

    #[test]
    fn stream_records_sink_client_pids_applications_and_titles() {
        let text = "\
Sink Input #172
	Driver: pipewire-pulse
	Sink: 48
	Media name: Jellyfin
	Properties:
\t\tmedia.name = \"Jellyfin\"
		application.name = \"Firefox\"
		application.process.id = \"4411\"
		client.id = \"52\"
		node.name = \"Firefox\"
";
        let streams = parse_sink_inputs(text);
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.iid, "172");
        assert_eq!(stream.sink.as_deref(), Some("48"));
        assert_eq!(stream.client.as_deref(), Some("52"));
        assert!(stream.pids.contains("4411"));
        assert!(stream.appkeys.contains("firefox"));
        assert!(stream.titles.contains("jellyfin"));
    }

    #[test]
    fn pipewire_own_pids_are_not_offered_as_owners() {
        let dump = r#"[
          {"id": 1, "type": "PipeWire:Interface:Client",
           "info": {"props": {"application.name": "pipewire-pulse", "pipewire.sec.pid": 900}}},
          {"id": 52, "type": "PipeWire:Interface:Client",
           "info": {"props": {"pipewire.sec.pid": 4411}}},
          {"id": 53, "type": "PipeWire:Interface:Client",
           "info": {"props": {"pipewire.sec.pid": 900}}}
        ]"#;
        let pids = client_pids(dump, &quiet);
        assert_eq!(pids.get("52").map(String::as_str), Some("4411"));
        assert!(!pids.contains_key("53"),
                "a stream owned by the pulse daemon must not look related to anything");
    }

    #[test]
    fn unreadable_dump_is_reported_not_swallowed() {
        let messages = std::cell::RefCell::new(Vec::new());
        let pids = client_pids("not json", &|msg| messages.borrow_mut().push(msg.to_string()));
        assert!(pids.is_empty());
        assert_eq!(messages.borrow().len(), 1);
    }
}
