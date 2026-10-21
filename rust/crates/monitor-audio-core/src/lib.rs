//! Monitor Audio's decisions, kept free of D-Bus and of the GUI so they can be
//! tested without a running session.
//!
//! The Python original is the specification this port is measured against; where
//! the two disagree, the Python behaviour is the bug to report, not to match.

pub mod config;
pub mod routing;
pub mod snapshot;

/// Everything that can go wrong is reported through this rather than printed from
/// inside the library, so the daemon can send it to the journal and a test can
/// assert that a failure was actually announced.
pub type Log<'a> = &'a dyn Fn(&str);

pub const BUS_NAME: &str = "local.MonitorAudio";
pub const OBJ_PATH: &str = "/local/MonitorAudio";
pub const IFACE: &str = "local.MonitorAudio";

/// Calm interval between passes over the streams.
pub const RESCAN_SECONDS: f64 = 4.0;
/// After an event that means a stream is about to appear, poll this fast...
pub const BURST_INTERVAL_MS: u64 = 150;
/// ...for this long.
pub const BURST_SECONDS: f64 = 5.0;

/// Name the silent keep-alive stream carries, so it can be recognised and hidden.
pub const KEEPALIVE_NAME: &str = "monitor-audio-keepalive";
/// How long one silent file plays for. paplay paces itself to the output and
/// exits when the file runs out, so the stream is renewed before this point: it
/// is a floor on how long a hold lasts, not a timer allowed to expire.
pub const KEEPALIVE_SECONDS: u64 = 1800;
pub const KEEPALIVE_RENEW_BEFORE: u64 = 60;

/// Minimum length before a window caption and a stream title may count as the
/// same thing; shorter strings collide far too easily.
pub const MIN_TITLE_LEN: usize = 5;
