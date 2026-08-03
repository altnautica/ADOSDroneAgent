//! Whether the store runs at all.
//!
//! The store is the largest writer on the box by a wide margin: measured on a
//! drone, 904 KB/s with it running and 49 KB/s with it stopped, so it accounts
//! for roughly 96% of everything written to the card. It also holds the largest
//! single lump of occupied space. It is a real feature and it will come back,
//! but until it costs less than it returns it ships **off**, behind one key.
//!
//! This is a deliberate capability regression, not an optimisation: with the
//! store off the node has no durable flight recorder, and `journalctl` is the
//! log of record. That is why journald stays `Storage=persistent` — with the
//! store gone it is the only thing left that survives a reboot.
//!
//! The gate is read in three places for one reason. The installer reads it to
//! decide whether to enable the unit, which is what makes "off" mean the daemon
//! never starts. The daemon reads it too, so that a unit started by hand, left
//! enabled by an older install, or pulled in by a dependency still declines to
//! create a store file. Neither on its own gives "off means genuinely off". The
//! storage diagnostic reads it third, so it can say "off" rather than reporting
//! an absent store as a fault.
//!
//! It lives in the shared config crate rather than beside the daemon so those
//! readers agree by construction: the daemon crate is far too heavy to pull into
//! an HTTP front just to answer one boolean, and a second hand-rolled parser is
//! how the two ends of one toggle drift apart.

use std::path::Path;

/// Where the agent config lives.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The parsed slice of `logging.store`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreGate {
    pub enabled: bool,
}

// Written out rather than derived, even though `false` is what a derive would
// produce. Which way this defaults is the whole decision, and a reader checking
// "is the store off unless asked for?" should find the answer stated here
// rather than have to know that `bool` derives to false.
#[allow(clippy::derivable_impls)]
impl Default for StoreGate {
    fn default() -> Self {
        // OFF. A fresh node, a node whose config predates this key, and a node
        // whose config cannot be parsed all land here.
        StoreGate { enabled: false }
    }
}

/// Parse `logging.store.enabled` out of a config body.
///
/// Absent or malformed resolves to **off**, and that direction is deliberate
/// even though it is the opposite of what every other gate in this codebase
/// does. The others default their feature ON because a config a box cannot read
/// must not silently disable a safety net. This one is not a safety net — it is
/// a recorder that costs 96% of the card's write volume, and defaulting it on
/// through a typo would hand a node back the exact behaviour that has been
/// destroying cards. Off is the recoverable direction: the operator loses
/// history they can turn back on with one key, rather than a card they have to
/// reflash.
pub fn read_gate_from(text: &str) -> StoreGate {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        logging: Logging,
    }
    #[derive(serde::Deserialize, Default)]
    struct Logging {
        #[serde(default)]
        store: Option<Store>,
    }
    #[derive(serde::Deserialize)]
    struct Store {
        #[serde(default)]
        enabled: bool,
    }

    let raw: Raw = crate::yaml_or_default(text, "logging.store");
    StoreGate {
        enabled: raw.logging.store.map(|s| s.enabled).unwrap_or(false),
    }
}

/// Whether the store is enabled by a config body. The boolean form, for callers
/// that want the answer rather than the record.
pub fn enabled_from(text: &str) -> bool {
    read_gate_from(text).enabled
}

/// Read the gate from a config file. A missing file is a fresh node, which is
/// off.
pub fn read_gate(path: &Path) -> StoreGate {
    match std::fs::read_to_string(path) {
        Ok(text) => read_gate_from(&text),
        Err(_) => StoreGate::default(),
    }
}

/// Read the gate from the canonical config path.
pub fn store_enabled() -> bool {
    read_gate(Path::new(CONFIG_YAML)).enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_node_has_the_store_off() {
        assert!(!read_gate_from("agent:\n  name: x\n").enabled);
        assert!(!StoreGate::default().enabled);
    }

    #[test]
    fn the_key_turns_it_on() {
        assert!(read_gate_from("logging:\n  store:\n    enabled: true\n").enabled);
    }

    #[test]
    fn the_key_turns_it_off_again() {
        assert!(!read_gate_from("logging:\n  store:\n    enabled: false\n").enabled);
    }

    #[test]
    fn a_config_that_does_not_parse_leaves_the_store_off() {
        // The opposite of every other gate here, on purpose: a typo must not be
        // able to hand a node back the write volume that was destroying cards.
        assert!(!read_gate_from(": : : not yaml").enabled);
        assert!(!read_gate_from("logging: [this, is, a, list]").enabled);
    }

    #[test]
    fn an_unrelated_logging_block_does_not_turn_it_on() {
        // The sibling keys under `logging:` predate this one and say nothing
        // about the store.
        assert!(!read_gate_from("logging:\n  level: debug\n  max_size_mb: 50\n").enabled);
    }

    #[test]
    fn a_missing_config_file_is_off_not_an_error() {
        assert!(!read_gate(Path::new("/no/such/config.yaml")).enabled);
    }
}
