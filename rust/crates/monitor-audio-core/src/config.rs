//! Settings the control panel writes, read the same tolerant way the Python
//! version read them: a broken file degrades to defaults rather than taking
//! routing down with it.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::Log;

pub const FALLBACK_DEFAULT: &str = "analog";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub keep_awake: bool,
    pub fallback_kind: String,
    /// display model -> text identifying the output to use
    pub manual_output: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            keep_awake: true,
            fallback_kind: FALLBACK_DEFAULT.to_string(),
            manual_output: BTreeMap::new(),
        }
    }
}

/// Python's truthiness, because the file a human may have edited by hand is
/// allowed to contain `0`, `""` or `null` where a switch belongs.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Read settings from `path`. Every failure mode returns usable defaults and
/// says out loud what was wrong; none of them are silent.
pub fn load_config(path: &Path, log: Log) -> Config {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(err) => {
            log(&format!("could not read {} ({}); using defaults", path.display(), err));
            return Config::default();
        }
    };

    let data: Value = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(err) => {
            log(&format!("{} is not valid json ({}); using defaults", path.display(), err));
            return Config::default();
        }
    };

    let object = match data.as_object() {
        Some(object) => object,
        None => {
            log(&format!("{} should hold a json object; using defaults", path.display()));
            return Config::default();
        }
    };

    let mut config = Config::default();
    if let Some(value) = object.get("enabled") {
        config.enabled = truthy(value);
    }
    if let Some(value) = object.get("keep_awake") {
        config.keep_awake = truthy(value);
    }
    if let Some(value) = object.get("fallback_kind") {
        match value.as_str() {
            Some(text) => config.fallback_kind = text.to_string(),
            None => log(&format!(
                "fallback_kind in {} should be a string; using {}",
                path.display(), FALLBACK_DEFAULT
            )),
        }
    }
    match object.get("manual_output") {
        None => {}
        Some(Value::Object(entries)) => {
            for (model, value) in entries {
                match value.as_str() {
                    Some(text) => {
                        config.manual_output.insert(model.clone(), text.to_string());
                    }
                    // Only strings are kept. A number or a list here cannot name
                    // an output, and the Python version died on one instead of
                    // ignoring it.
                    _ => log(&format!(
                        "manual_output[{}] in {} is not a string; ignoring it",
                        model,
                        path.display()
                    )),
                }
            }
        }
        Some(_) => log(&format!(
            "manual_output in {} should be an object; ignoring it",
            path.display()
        )),
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn quiet(_msg: &str) {}

    fn write_temp(name: &str, body: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("monitor-audio-config-test-{}-{}", std::process::id(), name));
        let mut file = std::fs::File::create(&path).expect("create temp settings");
        file.write_all(body.as_bytes()).expect("write temp settings");
        path
    }

    #[test]
    fn absent_file_gives_defaults() {
        let path = std::path::Path::new("/nonexistent/monitor-audio/config.json");
        assert_eq!(load_config(path, &quiet), Config::default());
    }

    #[test]
    fn invalid_json_gives_defaults_and_says_so() {
        let path = write_temp("bad", "{ not json");
        let messages = std::cell::RefCell::new(Vec::new());
        load_config(&path, &|msg| messages.borrow_mut().push(msg.to_string()));
        let messages = messages.borrow();
        assert_eq!(messages.len(), 1, "the failure must be reported: {:?}", messages);
        assert!(messages[0].contains("not valid json"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn json_that_is_not_an_object_gives_defaults() {
        let path = write_temp("array", "[1, 2, 3]");
        let config = load_config(&path, &quiet);
        assert_eq!(config, Config::default());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn wrong_types_fall_back_per_key_without_losing_the_rest() {
        let path = write_temp(
            "types",
            r#"{"enabled": false, "fallback_kind": 42, "manual_output": "nope", "keep_awake": 0}"#,
        );
        let config = load_config(&path, &quiet);
        assert!(!config.enabled, "the valid key must still be honoured");
        assert!(!config.keep_awake);
        assert_eq!(config.fallback_kind, FALLBACK_DEFAULT);
        assert!(config.manual_output.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn overrides_are_read_and_non_strings_dropped() {
        let path = write_temp(
            "overrides",
            r#"{"manual_output": {"SAMSUNG": "hdmi", "2778X": 7}}"#,
        );
        let config = load_config(&path, &quiet);
        assert_eq!(config.manual_output.get("SAMSUNG").map(String::as_str), Some("hdmi"));
        assert!(!config.manual_output.contains_key("2778X"));
        std::fs::remove_file(&path).ok();
    }
}
