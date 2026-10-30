//! Which monitors are plugged in, and what each one is called.
//!
//! The names have to match what the KWin script reports, because that is the key
//! the whole routing scheme is built on. Qt gets its `QScreen::model()` from the
//! monitor's EDID "monitor name" descriptor, so this reads the same bytes from
//! sysfs rather than asking a toolkit that is not here.

use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    pub model: String,
    pub connector: String,
}

/// Pull the advertised name out of an EDID block.
///
/// The four 18-byte descriptors start at 54, 72, 90 and 108; one of them is a
/// monitor name when its first three bytes are zero and the fourth is 0xFC. The
/// name itself is the fourteen bytes after a five-byte header, space padded.
pub fn monitor_name(edid: &[u8]) -> Option<String> {
    if edid.len() < 128 {
        return None;
    }
    for offset in [54usize, 72, 90, 108] {
        let block = &edid[offset..offset + 18];
        if block[0] == 0 && block[1] == 0 && block[3] == 0xFC {
            let name: String = block[5..18].iter().map(|byte| *byte as char).collect();
            let name = name.trim().trim_matches('\0').trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Every connected output, in a stable order.
///
/// A monitor that reports no name still gets a row, keyed by its connector, so a
/// display with a broken EDID can still be given an output by hand instead of
/// silently missing from the list.
pub fn connected_displays(sys_drm: &Path) -> Vec<Display> {
    let mut displays: Vec<Display> = Vec::new();
    let entries = match std::fs::read_dir(sys_drm) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("could not read {}: {}", sys_drm.display(), err);
            return displays;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let (_, connector) = match name.split_once('-') {
            Some((card, connector)) if card.starts_with("card") => (card, connector),
            _ => continue,
        };
        let directory = entry.path();
        let status = match std::fs::read_to_string(directory.join("status")) {
            Ok(status) => status.trim().to_string(),
            Err(err) => {
                eprintln!("could not read the status of {}: {}", name, err);
                continue;
            }
        };
        if status != "connected" {
            continue;
        }
        let model = match std::fs::read(directory.join("edid")) {
            Ok(edid) => monitor_name(&edid).unwrap_or_else(|| connector.to_string()),
            Err(_) => connector.to_string(),
        };
        displays.push(Display { model, connector: connector.to_string() });
    }
    displays.sort_by(|a, b| a.connector.cmp(&b.connector));
    displays
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edid_with_name(name: &str) -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        edid[54] = 0;
        edid[55] = 0;
        edid[56] = 0;
        edid[57] = 0xFC;
        let bytes = name.as_bytes();
        for (index, byte) in bytes.iter().enumerate().take(14) {
            edid[59 + index] = *byte;
        }
        edid
    }

    #[test]
    fn the_monitor_name_descriptor_is_read_back() {
        assert_eq!(monitor_name(&edid_with_name("MAG 275QF X30")).as_deref(), Some("MAG 275QF X30"));
    }

    #[test]
    fn a_shorter_name_is_padded_with_spaces_and_trimmed() {
        assert_eq!(monitor_name(&edid_with_name("SAMSUNG")).as_deref(), Some("SAMSUNG"));
    }

    #[test]
    fn an_edid_with_no_name_gives_nothing_rather_than_a_guess() {
        assert_eq!(monitor_name(&[0u8; 128]), None);
        assert_eq!(monitor_name(&[0u8; 10]), None, "a truncated block must not be parsed");
    }

    #[test]
    fn only_connected_outputs_are_listed() {
        let dir = tempfile::tempdir().expect("temp drm root");
        let root = dir.path();
        let on = root.join("card0-DP-2");
        let off = root.join("card0-HDMI-A-1");
        std::fs::create_dir_all(&on).unwrap();
        std::fs::create_dir_all(&off).unwrap();
        std::fs::write(on.join("status"), "connected\n").unwrap();
        std::fs::write(off.join("status"), "disconnected\n").unwrap();
        std::fs::write(on.join("edid"), edid_with_name("2778X")).unwrap();
        std::fs::write(off.join("edid"), edid_with_name("Gone TV")).unwrap();
        let displays = connected_displays(root);
        assert_eq!(displays.len(), 1, "got {:?}", displays);
        assert_eq!(displays[0].model, "2778X");
        assert_eq!(displays[0].connector, "DP-2");
    }

    #[test]
    fn a_connected_monitor_with_a_broken_edid_still_gets_a_row() {
        let dir = tempfile::tempdir().expect("temp drm root");
        let root = dir.path();
        let directory = root.join("card1-DP-1");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("status"), "connected\n").unwrap();
        let displays = connected_displays(root);
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].model, "DP-1", "the connector is better than an empty row");
    }
}
