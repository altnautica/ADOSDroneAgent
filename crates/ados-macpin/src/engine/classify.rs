//! Per-adapter pin decision.

use super::*;

/// The pure per-adapter decision. `learner` is the adapter's record AFTER the
/// caller has folded in this boot's observation (so `mac_change_count` is
/// current). `salt` is the adapter's stable USB path, so every pinned adapter
/// derives a MAC unique to its port and no two radios can collapse to one MAC.
/// Does no I/O.
#[allow(clippy::too_many_arguments)]
pub fn classify_adapter(
    usb_id: UsbId,
    key: &str,
    vidpid: &str,
    machine_id: Option<&str>,
    salt: &str,
    config: &ReconcileConfig,
    link_mechanism: bool,
    learner: Option<&LearnerRecord>,
    with_learner: bool,
) -> Decision {
    // 1. Operator override wins (by adapter key, else by model).
    if let Some(raw) = config
        .overrides
        .get(key)
        .or_else(|| config.overrides.get(vidpid))
    {
        return match (config.enabled, link_mechanism, MacAddr::parse(raw)) {
            (false, _, _) => Decision::Disabled,
            (true, false, _) => Decision::Deferred("no systemd-udev link mechanism".into()),
            (true, true, Some(mac)) => Decision::Pin {
                mac,
                source: AdapterSource::Override,
            },
            (true, true, None) => Decision::Deferred("override MAC is malformed".into()),
        };
    }

    // 2. Known-stable efuse radios are never touched.
    if is_known_stable_efuse(usb_id) {
        return Decision::Stable;
    }

    // 3. Known no-efuse randomizer (quirk table) -> auto-pin when armed.
    if is_quirk_randomizer(usb_id).is_some() {
        if !config.enabled {
            return Decision::Disabled;
        }
        if !link_mechanism {
            return Decision::Deferred("no systemd-udev link mechanism".into());
        }
        return match machine_id.and_then(|m| derive_pinned_mac(m, salt)) {
            Some(mac) => Decision::Pin {
                mac,
                source: AdapterSource::Quirk,
            },
            None => Decision::Deferred("no machine-id to derive a stable MAC".into()),
        };
    }

    // 4. Unknown adapter -> cross-boot learner (guided, never auto-pinned).
    if !with_learner {
        return Decision::Observe;
    }
    match learner {
        Some(rec) if rec.mac_change_count >= LEARN_THRESHOLD => {
            let proposed = machine_id.and_then(|m| derive_pinned_mac(m, salt));
            Decision::Candidate { proposed }
        }
        _ => Decision::Observe,
    }
}
