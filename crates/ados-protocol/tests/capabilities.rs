//! Structural checks on the generated agent capability catalog.
//!
//! The full byte-level fidelity against the Python catalog is asserted by the
//! codegen fidelity check; these guard the Rust view against accidental
//! truncation or a malformed regeneration.

use ados_protocol::capabilities as caps;
use std::collections::BTreeSet;

#[test]
fn catalog_is_non_empty_and_ids_are_unique() {
    assert!(!caps::AGENT_CAPABILITIES.is_empty());
    let ids: BTreeSet<&str> = caps::AGENT_CAPABILITIES.iter().map(|c| c.id).collect();
    assert_eq!(
        ids.len(),
        caps::AGENT_CAPABILITIES.len(),
        "duplicate capability id"
    );
}

#[test]
fn enforced_is_a_subset_and_contains_the_event_bus() {
    let enforced: BTreeSet<&str> = caps::enforced_agent_capabilities().collect();
    // Every enforced id is a known capability.
    for id in &enforced {
        assert!(
            caps::is_known_agent_capability(id),
            "enforced cap {id} not in catalog"
        );
    }
    // The event bus is the enforced pair today.
    assert!(enforced.contains("event.publish"));
    assert!(enforced.contains("event.subscribe"));
}

#[test]
fn spot_check_known_capabilities_and_risk() {
    let write = caps::get_agent_capability("mavlink.write").expect("mavlink.write present");
    assert_eq!(write.category, "flight_control");
    assert_eq!(write.risk, "high");
    assert!(!write.enforced);

    let pubcap = caps::get_agent_capability("event.publish").expect("event.publish present");
    assert!(pubcap.enforced);

    assert!(!caps::is_known_agent_capability("does.not.exist"));
    assert!(caps::get_agent_capability("does.not.exist").is_none());
}
