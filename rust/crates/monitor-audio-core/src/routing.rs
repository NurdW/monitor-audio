//! The decisions: which output a display owns, and which stream a window owns.
//!
//! These are the two functions worth getting right, and both are deliberately
//! conservative. A wrong output is audible; a wrong guess about ownership moves
//! someone's audio out from under them.

use std::collections::{BTreeSet, HashMap};

use crate::config::Config;
use crate::snapshot::{Sink, Stream};
use crate::Log;
use crate::MIN_TITLE_LEN;

/// Fold a name into something comparable across sources.
///
/// A stream says "Firefox" while the window says "org.mozilla.firefox", so for a
/// reverse-DNS id the last part is what has to be compared.
pub fn norm(value: &str) -> String {
    let raw = value.to_lowercase();
    let parts: Vec<&str> = raw.split(['.', '-']).collect();
    let name = if parts.len() >= 3 {
        parts[parts.len() - 1]
    } else {
        raw.as_str()
    };
    let cleaned = alnum_only(name);
    if cleaned.is_empty() {
        alnum_only(&raw)
    } else {
        cleaned
    }
}

fn alnum_only(value: &str) -> String {
    value.chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// What the desktop told us about one window: enough to recognise its audio.
#[derive(Clone, Debug, Default)]
pub struct Route {
    pub pid: String,
    pub model: String,
    pub appkeys: BTreeSet<String>,
    pub captions: BTreeSet<String>,
}

/// Which audio output belongs to the display called `model`, and how that was
/// decided. Re-decided on every pass, so unplugging a display or changing the
/// sound card profile needs no restart.
///
/// Order matters: an explicit choice wins, then a display whose model an output
/// names, then the fallback. Matching is on the display's model rather than on a
/// sink name because sink names change when the profile changes and models do not.
pub fn resolve_output(model: &str, sinks: &[Sink], config: &Config, log: Log) -> (Option<String>, String) {
    if let Some(override_text) = config.manual_output.get(model) {
        let needle = norm(override_text);
        if !needle.is_empty() {
            let hits: Vec<&Sink> = sinks
                .iter()
                .filter(|sink| {
                    norm(&sink.name).contains(&needle) || norm(&sink.description).contains(&needle)
                })
                .collect();
            if let Some(hit) = hits.first() {
                return (Some(hit.name.clone()), "override".to_string());
            }
            let names: Vec<&str> = sinks.iter().map(|sink| sink.name.as_str()).collect();
            log(&format!(
                "override '{}' matches no output; outputs are: {}",
                override_text,
                if names.is_empty() { "none".to_string() } else { names.join(", ") }
            ));
            return (None, "override-miss".to_string());
        }
    }

    let key = norm(model);
    if !key.is_empty() {
        let hits: Vec<&Sink> = sinks
            .iter()
            .filter(|sink| norm(&sink.description).contains(&key))
            .collect();
        match hits.len() {
            0 => {}
            1 => return (Some(hits[0].name.clone()), "display".to_string()),
            _ => {
                let names: Vec<&str> = hits.iter().map(|sink| sink.name.as_str()).collect();
                log(&format!(
                    "display '{}' matches several outputs ({}); using {}",
                    model,
                    names.join(", "),
                    hits[0].name
                ));
                return (Some(hits[0].name.clone()), "display".to_string());
            }
        }
    }

    if let Some(fallback) = sinks.iter().find(|sink| sink.name.contains(&config.fallback_kind)) {
        return (Some(fallback.name.clone()), "fallback".to_string());
    }
    (None, "none".to_string())
}

/// Every process id that can claim this stream.
pub fn owner_pids(stream: &Stream, pids_of_client: &HashMap<String, String>) -> BTreeSet<String> {
    let mut pids = stream.pids.clone();
    if let Some(client) = &stream.client {
        if let Some(owner) = pids_of_client.get(client) {
            pids.insert(owner.clone());
        }
    }
    pids
}

/// Does this stream's tab title belong to this window?
///
/// Only meaningful when one process owns several windows: every window of a
/// browser shares a pid and an application name, so the tab title is the single
/// thing that says which window a stream came from.
pub fn title_match(stream: &Stream, route: &Route) -> bool {
    stream.titles.iter().any(|title| {
        title.chars().count() >= MIN_TITLE_LEN
            && route.captions.iter().any(|caption| {
                caption.chars().count() >= MIN_TITLE_LEN
                    && (caption.contains(title.as_str()) || title.contains(caption.as_str()))
            })
    })
}

/// Fallback for streams that expose no usable pid at all.
pub fn name_match(stream: &Stream, route: &Route) -> bool {
    let stream_apps: BTreeSet<&String> = stream.appkeys.iter().filter(|k| k.chars().count() >= 3).collect();
    let route_apps: BTreeSet<&String> = route.appkeys.iter().filter(|k| k.chars().count() >= 3).collect();
    stream_apps.iter().any(|key| route_apps.contains(key))
}

/// Is this stream this window's audio? Never answers by guessing.
pub fn stream_belongs(
    stream: &Stream,
    wid: &str,
    route: &Route,
    pids_of_client: &HashMap<String, String>,
    wanted: &HashMap<String, Route>,
) -> bool {
    let owners = owner_pids(stream, pids_of_client);
    if owners.contains(&route.pid) {
        // Same process owns several routed windows: only the tab title may decide.
        let rivals = wanted
            .iter()
            .any(|(other, other_route)| other != wid && other_route.pid == route.pid);
        return if rivals { title_match(stream, route) } else { true };
    }
    owners.is_empty() && name_match(stream, route)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::snapshot::parse_sinks;

    fn quiet(_msg: &str) {}

    const SINKS: &str = "\
Sink #47
	Name: alsa_output.pci-0000_09_00.4.analog-stereo
	Description: Starship/Matisse HD Audio Controller Analog Stereo
Sink #48
	Name: alsa_output.pci-0000_07_00.1.hdmi-stereo
	Description: Navi 21/23 HDMI/DP Audio Controller Digital Stereo (HDMI) [SAMSUNG]
";

    fn sinks() -> Vec<Sink> {
        parse_sinks(SINKS)
    }

    #[test]
    fn reverse_dns_ids_compare_on_their_last_part() {
        assert_eq!(norm("org.mozilla.firefox"), "firefox");
        assert_eq!(norm("Firefox"), "firefox");
        // Only a reverse-DNS id (three or more dotted parts) shortens to its last
        // part; a hyphenated name keeps both halves, which is what the Python
        // original did and what a name like "firefox-firefox" needs.
        assert_eq!(norm("firefox-firefox"), "firefoxfirefox");
        assert_eq!(norm("org.gnome.Nautilus"), "nautilus");
        assert_eq!(norm("VLC"), "vlc");
        assert_eq!(norm(""), "");
    }

    #[test]
    fn a_display_named_in_an_output_owns_that_output() {
        let (name, how) = resolve_output("SAMSUNG", &sinks(), &Config::default(), &quiet);
        assert_eq!(how, "display");
        assert_eq!(name.as_deref(), Some("alsa_output.pci-0000_07_00.1.hdmi-stereo"));
    }

    #[test]
    fn a_display_with_no_audio_of_its_own_uses_the_fallback() {
        let (name, how) = resolve_output("MAG 275QF X30", &sinks(), &Config::default(), &quiet);
        assert_eq!(how, "fallback");
        assert_eq!(name.as_deref(), Some("alsa_output.pci-0000_09_00.4.analog-stereo"));
    }

    #[test]
    fn an_override_beats_the_model_match() {
        let mut config = Config::default();
        config.manual_output.insert("SAMSUNG".into(), "analog".into());
        let (name, how) = resolve_output("SAMSUNG", &sinks(), &config, &quiet);
        assert_eq!(how, "override");
        assert_eq!(name.as_deref(), Some("alsa_output.pci-0000_09_00.4.analog-stereo"));
    }

    #[test]
    fn an_override_naming_nothing_says_so_and_routes_nothing() {
        let mut config = Config::default();
        config.manual_output.insert("SAMSUNG".into(), "nonexistent-card".into());
        let messages = std::cell::RefCell::new(Vec::new());
        let (name, how) = resolve_output(
            "SAMSUNG", &sinks(), &config, &|msg| messages.borrow_mut().push(msg.to_string()));
        assert_eq!(how, "override-miss");
        assert_eq!(name, None, "a stale override must not silently fall back");
        let messages = messages.borrow();
        assert_eq!(messages.len(), 1, "the miss has to be reported: {:?}", messages);
    }

    #[test]
    fn no_outputs_at_all_is_reported_as_such() {
        let (name, how) = resolve_output("SAMSUNG", &[], &Config::default(), &quiet);
        assert_eq!((name, how.as_str()), (None, "none"));
    }

    #[test]
    fn short_titles_do_not_count_as_a_match() {
        let mut stream = Stream { iid: "1".into(), ..Default::default() };
        stream.titles.insert("ab".into());
        let mut route = Route { pid: "1".into(), ..Default::default() };
        route.captions.insert("abcd".into());
        assert!(!title_match(&stream, &route), "two two-letter strings are not the same tab");
    }

    #[test]
    fn a_real_tab_title_matches_its_window() {
        let mut stream = Stream { iid: "1".into(), ..Default::default() };
        stream.titles.insert("jellyfin".into());
        let mut route = Route { pid: "1".into(), ..Default::default() };
        // Captions arrive normed from the desktop integration, the same way
        // application names do; title_match compares folded text with folded text.
        route.captions.insert("jellyfin - stranger things".into());
        assert!(title_match(&stream, &route));
    }

    #[test]
    fn a_stream_with_an_owner_that_is_not_this_window_is_not_moved() {
        let mut stream = Stream { iid: "1".into(), ..Default::default() };
        stream.pids.insert("999".into());
        let route = Route { pid: "111".into(), ..Default::default() };
        assert!(!stream_belongs(&stream, "w1", &route, &HashMap::new(), &HashMap::new()));
    }

    #[test]
    fn an_ownerless_stream_is_only_moved_when_the_application_agrees() {
        let mut stream = Stream { iid: "1".into(), ..Default::default() };
        stream.appkeys.insert("firefox".into());
        let mut route = Route { pid: "111".into(), ..Default::default() };
        route.appkeys.insert("firefox".into());
        assert!(stream_belongs(&stream, "w1", &route, &HashMap::new(), &HashMap::new()));

        let mut other = Route { pid: "111".into(), ..Default::default() };
        other.appkeys.insert("vlc".into());
        assert!(!stream_belongs(&stream, "w1", &other, &HashMap::new(), &HashMap::new()));
    }

    #[test]
    fn when_one_process_owns_several_windows_only_the_title_decides() {
        let mut stream = Stream { iid: "1".into(), ..Default::default() };
        stream.pids.insert("111".into());
        stream.titles.insert("stranger things".into());

        let mut route = Route { pid: "111".into(), ..Default::default() };
        route.captions.insert("Stranger Things 4k".into());

        let mut one_window = HashMap::new();
        one_window.insert("w1".to_string(), route.clone());
        assert!(stream_belongs(&stream, "w1", &route, &HashMap::new(), &one_window));

        // A second window from the same process: the title is now the only thing
        // that may claim the stream, and this window's title does not match.
        let mut two_windows = HashMap::new();
        two_windows.insert("w1".to_string(), route.clone());
        two_windows.insert("w2".to_string(), Route { pid: "111".into(), ..Default::default() });
        assert!(!stream_belongs(&stream, "w2", &two_windows["w2"], &HashMap::new(), &two_windows));
    }
}
