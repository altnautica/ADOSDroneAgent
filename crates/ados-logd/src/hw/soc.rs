//! SoC family detection from the device-tree compatible list.
//!
//! Read once at collector start. `/proc/device-tree/compatible` is a
//! NUL-separated list of compatible strings, most-specific first. The family is
//! used to gate SoC-specific signals: the Pi throttle flags come from the
//! Broadcom `vcgencmd` only on the Pi/BCM family, so a non-Pi board never spawns
//! a missing binary.
//!
//! The read is root-injectable so a test points it at a fixture; the parse
//! mirrors the device-tree NUL-list convention used elsewhere in the agent.

use std::path::Path;

use super::reader::under;

/// The coarse SoC family the collector gates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocFamily {
    /// Broadcom (Raspberry Pi). Exposes the `vcgencmd` throttle interface.
    Broadcom,
    /// Any other / unidentified family.
    Other,
}

/// A detected SoC: the family plus the most-specific compatible string (for the
/// snapshot's `soc.compat` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocInfo {
    /// The gating family.
    pub family: SocFamily,
    /// The most-specific compatible string, or empty when none was readable.
    pub compat: String,
}

/// Detect the SoC family by reading `/proc/device-tree/compatible` under `root`.
/// Returns [`SocFamily::Other`] with an empty `compat` when the node is absent.
pub fn detect_soc(root: &Path) -> SocInfo {
    let path = under(root, "/proc/device-tree/compatible");
    let raw = std::fs::read(&path).unwrap_or_default();
    let compatibles = parse_compatible(&raw);
    let family = if compatibles
        .iter()
        .any(|c| c.starts_with("brcm,") || c.contains("bcm2"))
    {
        SocFamily::Broadcom
    } else {
        SocFamily::Other
    };
    SocInfo {
        family,
        compat: compatibles.into_iter().next().unwrap_or_default(),
    }
}

/// Split the raw device-tree compatible bytes into the list of compatible
/// strings. The node is a sequence of NUL-terminated strings (the last is also
/// NUL-terminated), so the trailing empty element and any empty run are dropped.
fn parse_compatible(raw: &[u8]) -> Vec<String> {
    raw.split(|&b| b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}

#[cfg(test)]
mod tests;
