// SPDX-License-Identifier: GPL-3.0-or-later

//! Which display is wired to which sound card, read from the kernel.
//!
//! PipeWire writes a monitor's name into an output description only while that
//! link is up, and the ALSA profile - and with it the output's whole name -
//! changes whenever a monitor is plugged in or drops out. Matching a display to
//! an output by description or by sink name therefore breaks exactly when a
//! display comes or goes, which is when the answer matters most.
//!
//! ELD is the kernel's own record of which display sits on which HDMI/DP audio
//! pin, so it answers the question without depending on either of those.

use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HdmiLink {
    /// The display's reported model, exactly as the display states it.
    pub model: String,
    /// PCI slot of the sound card carrying that display, e.g. `0000:07:00.1`.
    pub card_slot: String,
}

/// Read every present, valid ELD record under `proc_asound`, pairing each with the
/// PCI slot of the card it belongs to.
///
/// Both roots are arguments so this can be tested without touching a real machine.
/// Anything unreadable is skipped: a machine with no ELD at all simply gets no
/// links, and the caller falls back to matching on descriptions as before.
pub fn read_links(proc_asound: &Path, sys_sound: &Path) -> Vec<HdmiLink> {
    let mut links = Vec::new();
    let Ok(cards) = std::fs::read_dir(proc_asound) else {
        return links;
    };
    for card in cards.flatten() {
        let card_name = card.file_name().to_string_lossy().to_string();
        let Some(digits) = card_name.strip_prefix("card") else {
            continue;
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Some(slot) = card_slot(sys_sound, &card_name) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(card.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("eld#") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            // A pin with nothing on it, or with a display that never sent its ELD,
            // tells us nothing about which monitor is there.
            if field(&text, "monitor_present") != "1" || field(&text, "eld_valid") != "1" {
                continue;
            }
            let model = field(&text, "monitor_name");
            if model.is_empty() {
                continue;
            }
            links.push(HdmiLink { model, card_slot: slot.clone() });
        }
    }
    links
}

/// ELD files are `key<TAB>value` lines.
fn field(text: &str, key: &str) -> String {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(key) {
            return parts.next().unwrap_or_default().to_string();
        }
    }
    String::new()
}

fn card_slot(sys_sound: &Path, card: &str) -> Option<String> {
    let target = std::fs::read_link(sys_sound.join(card).join("device")).ok()?;
    let slot = target.file_name()?.to_string_lossy().to_string();
    // A real PCI device link ends in the slot; a USB sound card ends in something
    // like "1-2:1.2", which no ALSA sink name can match anyway.
    if slot.split(':').next().is_some_and(|head| head.len() == 4) {
        Some(slot)
    } else {
        None
    }
}

/// The PCI slot an ALSA sink name was built from:
/// `alsa_output.pci-0000_07_00.1.hdmi-stereo` becomes `0000:07:00.1`.
pub fn sink_slot(sink_name: &str) -> Option<String> {
    let rest = sink_name.strip_prefix("alsa_output.pci-")?;
    let mut parts = rest.split('.');
    let left = parts.next()?;
    let function = parts.next()?;
    let bits: Vec<&str> = left.split('_').collect();
    if bits.len() != 3 {
        return None;
    }
    Some(format!("{}:{}:{}.{}", bits[0], bits[1], bits[2], function))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sink_name_folds_back_to_the_pci_slot_it_came_from() {
        assert_eq!(
            sink_slot("alsa_output.pci-0000_07_00.1.hdmi-stereo").as_deref(),
            Some("0000:07:00.1")
        );
        assert_eq!(
            sink_slot("alsa_output.pci-0000_09_00.4.analog-stereo").as_deref(),
            Some("0000:09:00.4")
        );
        assert_eq!(sink_slot("easyeffects_sink"), None);
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ma-eld-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn only_a_present_display_with_valid_eld_counts_as_a_link() {
        let root = scratch("links");
        let proc_asound = root.join("asound");
        let sys_sound = root.join("sound");
        // card0 is the graphics card: two live displays and two dead pins.
        std::fs::create_dir_all(proc_asound.join("card0")).expect("card0");
        std::fs::create_dir_all(sys_sound.join("card0")).expect("card0 sysfs");
        std::os::unix::fs::symlink("../../../0000:07:00.1", sys_sound.join("card0").join("device"))
            .expect("graphics card device link");
        std::fs::write(
            proc_asound.join("card0").join("eld#0.2"),
            "monitor_present\t\t1\neld_valid\t\t1\nmonitor_name\t\tSAMSUNG\nconnection_type\t\tDisplayPort\n",
        )
        .expect("eld 2");
        std::fs::write(
            proc_asound.join("card0").join("eld#0.3"),
            "monitor_present\t\t0\neld_valid\t\t0\n",
        )
        .expect("eld 3");
        // card1 has a present display but its device link is not a PCI device.
        std::fs::create_dir_all(proc_asound.join("card1")).expect("card1");
        std::fs::create_dir_all(sys_sound.join("card1")).expect("card1 sysfs");
        std::os::unix::fs::symlink("../../1-2:1.2", sys_sound.join("card1").join("device"))
            .expect("usb sound card device link");
        std::fs::write(
            proc_asound.join("card1").join("eld#1.0"),
            "monitor_present\t\t1\neld_valid\t\t1\nmonitor_name\t\tUSB HEADSET\n",
        )
        .expect("eld usb");

        let links = read_links(&proc_asound, &sys_sound);
        assert_eq!(links.len(), 1, "dead pins and non-PCI cards are skipped: {:?}", links);
        assert_eq!(links[0].model, "SAMSUNG");
        assert_eq!(links[0].card_slot, "0000:07:00.1");
    }

    #[test]
    fn a_machine_with_no_eld_at_all_yields_no_links_instead_of_failing() {
        let root = scratch("empty");
        assert!(read_links(&root.join("missing"), &root).is_empty());
    }
}
