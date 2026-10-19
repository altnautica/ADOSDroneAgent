//! Reconciliation.

use super::*;

/// True when a fingerprint carries no usable field (an empty `match: {}` the HAL
/// writes when it could not read a fingerprint).
pub(super) fn match_is_empty(m: Option<&CameraMatch>) -> bool {
    match m {
        None => true,
        Some(m) => m.usb.is_none() && m.csi_sensor.is_none() && m.csi_port.is_none(),
    }
}

/// True when a declared leg's fingerprint identifies the same physical device as
/// a discovered one: an equal USB `vid:pid[:serial]`, or an equal CSI sensor +
/// port. Empty fingerprints never match (they identify no device).
pub(super) fn fingerprint_matches(
    declared: Option<&CameraMatch>,
    discovered: Option<&CameraMatch>,
) -> bool {
    let (Some(d), Some(x)) = (declared, discovered) else {
        return false;
    };
    if let (Some(a), Some(b)) = (&d.usb, &x.usb) {
        if a == b {
            return true;
        }
    }
    if d.csi_sensor.is_some() && d.csi_sensor == x.csi_sensor && d.csi_port == x.csi_port {
        return true;
    }
    false
}

/// True when a leg source is a network capture URL (a leg mediamtx pulls, not a
/// local device the discovery covers). Recognises the plain and TLS forms of
/// RTSP and HTTP, matching the pipeline's own `CameraLeg::network_source` so the
/// roster and the pipeline classify a leg identically.
pub(super) fn is_network_source(source: &str) -> bool {
    ados_video::config::is_network_url(source)
}

/// True when a leg source is a bare device-class hint (`csi` / `usb` / `ip`)
/// rather than a concrete `/dev/videoN` path or a URL — the shape the legacy
/// single-camera block carries.
pub(super) fn is_hint_source(source: &str) -> bool {
    matches!(source.trim(), "csi" | "usb" | "ip")
}

/// Find the index of an unused discovered device this local leg identifies, or
/// `None`. Matches a concrete path by `device_path == source`, else by
/// fingerprint (a hot-plug renamed the node → re-pin); matches a bare hint source
/// by device class. Never matches a network leg (no discovery covers it).
pub(super) fn match_discovered(
    leg: &CameraLeg,
    discovered: &[DiscoveredCamera],
    used: &[bool],
) -> Option<usize> {
    let source = leg.source.trim();
    if is_network_source(source) {
        return None;
    }
    if is_hint_source(source) {
        // A bare device-class hint (the legacy single-camera block's `csi` / `usb`
        // / `ip`). Prefer a device of the hinted class, then fall through the
        // pipeline's CSI→USB→IP auto-assign order (`camera_mgr.auto_assign`) so a
        // `csi`-hint main leg on a node whose only camera is USB reconciles to that
        // USB device — assigned, one row — rather than showing offline while the
        // USB camera appears as a phantom unassigned row.
        for class in [source, "csi", "usb", "ip"] {
            if let Some(i) = discovered
                .iter()
                .enumerate()
                .find_map(|(i, d)| (!used[i] && d.cam_type == class).then_some(i))
            {
                return Some(i);
            }
        }
        return None;
    }
    // Concrete path: exact device-path match first.
    if let Some(i) = discovered
        .iter()
        .enumerate()
        .find_map(|(i, d)| (!used[i] && d.device_path == source).then_some(i))
    {
        return Some(i);
    }
    // Else re-pin by fingerprint (the device moved to a different node).
    discovered.iter().enumerate().find_map(|(i, d)| {
        (!used[i] && fingerprint_matches(leg.camera_match.as_ref(), d.camera_match.as_ref()))
            .then_some(i)
    })
}

/// The owner tag, treating an absent or `"operator"` owner as operator-managed and
/// anything else as a plugin id.
pub(super) fn is_plugin_owned(owner: Option<&str>) -> bool {
    matches!(owner, Some(o) if !o.is_empty() && o != "operator")
}

/// A JSON string value, or `null` for an absent / empty option.
pub(super) fn opt_str(value: Option<&str>) -> Value {
    match value {
        Some(s) if !s.is_empty() => Value::from(s),
        _ => Value::Null,
    }
}

/// Serialize an optional fingerprint into the roster row, treating an empty one
/// as absent (`null`).
pub(super) fn match_value(m: Option<&CameraMatch>) -> Value {
    if match_is_empty(m) {
        return Value::Null;
    }
    serde_json::to_value(m.expect("checked non-empty")).unwrap_or(Value::Null)
}

/// Reconcile declared legs + discovered devices + live state into the roster rows.
///
/// Every declared leg becomes a row (state `assigned` / `plugin_owned` when its
/// device is present or it is a network leg, `offline` when a declared local
/// device is not discovered). Every discovered device not claimed by a declared
/// leg becomes a `discovered_unassigned` row so the operator can assign it. Pure
/// (no I/O) so the full reconciliation matrix is unit-tested.
pub(super) fn build_roster(
    declared: &[CameraLeg],
    discovered: &[DiscoveredCamera],
    live: &HashMap<String, Option<bool>>,
) -> Vec<Value> {
    // The primary declared leg is served at the fixed `main` path, so its live
    // state is keyed on `main` in the sidecar (matching `resolve_legs`).
    let primary_idx = declared
        .iter()
        .position(|l| l.role.as_deref() == Some("primary"));
    let mut used = vec![false; discovered.len()];
    let mut rows: Vec<Value> = Vec::new();

    for (i, leg) in declared.iter().enumerate() {
        let is_primary = match primary_idx {
            Some(p) => i == p,
            None => i == 0,
        };
        let served_id = if is_primary { "main" } else { leg.id.as_str() };

        let network = is_network_source(&leg.source);
        let mut device_path = if !network && leg.source.trim().contains('/') {
            // A concrete configured device path is the leg's intended device even
            // when it is currently absent.
            Some(leg.source.trim().to_string())
        } else {
            None
        };
        let mut match_val = leg.camera_match.clone();
        let mut present = network;

        if !network {
            if let Some(di) = match_discovered(leg, discovered, &used) {
                used[di] = true;
                let d = &discovered[di];
                device_path = Some(d.device_path.clone());
                if match_is_empty(match_val.as_ref()) {
                    match_val = d.camera_match.clone();
                }
                present = true;
            }
        }

        let state = if !present {
            "offline"
        } else if is_plugin_owned(leg.owner.as_deref()) {
            "plugin_owned"
        } else {
            "assigned"
        };

        // An offline leg is not live, regardless of a stale live-stream sidecar
        // reading — only a present leg reports its sidecar liveness (so the state
        // and live fields never contradict each other).
        let live_state = if present {
            live.get(served_id)
                .copied()
                .flatten()
                .or_else(|| live.get(&leg.id).copied().flatten())
        } else {
            None
        };

        rows.push(json!({
            "id": leg.id,
            "name": opt_str(leg.name.as_deref()),
            "source": leg.source,
            "role": opt_str(leg.role.as_deref()),
            "purpose": leg.purpose,
            "orientation": opt_str(leg.orientation.as_deref()),
            "enabled": leg.enabled,
            "owner": opt_str(leg.owner.as_deref()),
            "state": state,
            "live": live_state.map(Value::from).unwrap_or(Value::Null),
            "device_path": device_path.map(Value::from).unwrap_or(Value::Null),
            "width": leg.width,
            "height": leg.height,
            "fps": leg.fps,
            "codec": leg.codec,
            "bitrate_kbps": leg.bitrate_kbps,
            "match": match_value(match_val.as_ref()),
            "fov_deg": leg.fov_deg.map(Value::from).unwrap_or(Value::Null),
            "mount_pitch_deg": leg.mount_pitch_deg.map(Value::from).unwrap_or(Value::Null),
            "calibration": opt_str(leg.calibration.as_deref()),
        }));
    }

    // Discovered devices no declared leg claimed: unassigned candidates the
    // operator can add.
    for (i, d) in discovered.iter().enumerate() {
        if used[i] {
            continue;
        }
        let id = device_handle(&d.device_path);
        rows.push(json!({
            "id": id,
            "name": opt_str(Some(d.name.as_str())),
            "source": d.device_path,
            "role": Value::Null,
            "purpose": Vec::<String>::new(),
            "orientation": Value::Null,
            "enabled": false,
            "owner": Value::Null,
            "state": "discovered_unassigned",
            "live": Value::Null,
            "device_path": opt_str(Some(d.device_path.as_str())),
            "width": (d.width > 0).then_some(d.width).map(Value::from).unwrap_or(Value::Null),
            "height": (d.height > 0).then_some(d.height).map(Value::from).unwrap_or(Value::Null),
            "fps": Value::Null,
            "codec": Value::Null,
            "bitrate_kbps": Value::Null,
            "match": match_value(d.camera_match.as_ref()),
            "fov_deg": Value::Null,
            "mount_pitch_deg": Value::Null,
            "calibration": Value::Null,
        }));
    }

    rows
}

/// The `cameras[]` entry keys every roster row carries verbatim from its config
/// entry (`null` when the entry does not set the key, or the row has no entry).
pub(super) const ENTRY_KEYS: [&str; 11] = [
    "capture",
    "format",
    "exposure_us",
    "gain",
    "trigger",
    "trigger_channel",
    "keyframe_interval",
    "rotation",
    "hflip",
    "vflip",
    "encoder",
];

/// Copy [`ENTRY_KEYS`] from each declared leg's raw config entry (keyed by id)
/// onto its roster row. A discovered-unassigned row, or a leg with no config
/// entry (the synthesised legacy `main`), gets `null` for every key.
pub(super) fn attach_entry_keys(rows: &mut [Value], entries: &HashMap<String, Value>) {
    for row in rows {
        let entry = if row["state"] == "discovered_unassigned" {
            None
        } else {
            row["id"].as_str().and_then(|id| entries.get(id))
        };
        let Some(obj) = row.as_object_mut() else {
            continue;
        };
        for key in ENTRY_KEYS {
            let v = entry
                .and_then(|e| e.get(key))
                .cloned()
                .unwrap_or(Value::Null);
            obj.insert(key.to_string(), v);
        }
    }
}

/// A stable roster handle for an unassigned discovered device: the device-node
/// basename (`/dev/video0` → `video0`), or the whole path when it has no `/`.
pub(super) fn device_handle(device_path: &str) -> String {
    device_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(device_path)
        .to_string()
}
