//! Link presets and the radio backend choice read from `video.wfb`.

use serde::{Deserialize, Serialize};

/// Operator-facing radio link presets, each mapping to the trio of wfb-ng
/// tunables `(mcs_index, fec_k, fec_n)`. Tuned for RTL8812EU radios on a 20 MHz
/// channel. Values are byte-identical to the Python `_LINK_PRESETS` table.
///
/// - `conservative` MCS 1, FEC 8/12 (50% redundancy). The default; robust under
///   low SNR / host-vbus power budgets / a noisy bench.
/// - `balanced` MCS 3, FEC 8/12 (50% redundancy). Headroom for outdoor links
///   where SNR is reliably above ~10 dB.
/// - `aggressive` MCS 5, FEC 8/10 (25% redundancy). Excellent SNR + close-in.
///
/// Returns `None` for an unknown preset name (caller keeps the current config).
pub fn link_preset_trio(preset: &str) -> Option<(u8, u8, u8)> {
    match preset {
        "conservative" => Some((1, 8, 12)),
        "balanced" => Some((3, 8, 12)),
        "aggressive" => Some((5, 8, 10)),
        _ => None,
    }
}

/// Which radio backend drives the WFB link. `kernel` is the
/// Linux monitor-mode + `wfb_tx`/`wfb_rx` backend (the SBC default); `userspace`
/// selects the cross-platform devourer USB backend (built only under the
/// `userspace-usb` feature; future); `auto` (the default) resolves to `kernel` on
/// Linux. Read from `video.wfb.backend`. Parsing is permissive: an unknown value
/// reads as `auto` rather than failing the config load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendChoice {
    /// Force the kernel monitor-mode backend.
    Kernel,
    /// Force the userspace USB (devourer) backend.
    Userspace,
    /// Resolve automatically (kernel on Linux). The default.
    #[default]
    Auto,
}

impl BackendChoice {
    /// Parse the wire string. Anything other than `kernel` / `userspace`
    /// (case-insensitive) reads as `auto`, so a fresh / malformed value is
    /// permissive rather than fail-closed.
    pub fn from_wire(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "kernel" => BackendChoice::Kernel,
            "userspace" => BackendChoice::Userspace,
            _ => BackendChoice::Auto,
        }
    }

    /// The wire string persisted in config + surfaced on the sidecar.
    pub fn as_wire(self) -> &'static str {
        match self {
            BackendChoice::Kernel => "kernel",
            BackendChoice::Userspace => "userspace",
            BackendChoice::Auto => "auto",
        }
    }
}

impl<'de> Deserialize<'de> for BackendChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(BackendChoice::from_wire(&s))
    }
}

impl Serialize for BackendChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_wire())
    }
}
