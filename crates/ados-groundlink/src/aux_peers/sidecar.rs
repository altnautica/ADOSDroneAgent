use super::*;

/// Read the per-slot flight-controller system ids out of a relayed-status
/// document, re-aged against `now`.
///
/// A document whose writer stamp is older than `max_doc_age_s` (or from the
/// future by more than a second) yields nothing: a stopped receive process
/// must not keep asserting identities it no longer hears. Each row is
/// re-aged against [`SYSTEM_ID_STALE_AFTER_S`] too, and the conflict flag is
/// recomputed from the surviving rows rather than trusted from the writer.
pub fn slot_system_ids_from_sidecar(
    doc: &Value,
    now: f64,
    max_doc_age_s: f64,
) -> BTreeMap<u8, SlotSystemId> {
    let mut out = BTreeMap::new();
    let written_at = doc
        .get("wall_time_unix")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let age = now - written_at;
    if written_at <= 0.0 || !(-1.0..=max_doc_age_s).contains(&age) {
        return out;
    }
    let rows = doc
        .get("fleet_system_ids")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for row in rows {
        let slot = row.get("slot").and_then(Value::as_u64);
        let id = row.get("fc_system_id").and_then(Value::as_u64);
        let seen = row.get("seen_at_unix").and_then(Value::as_f64);
        let (Some(slot), Some(id), Some(seen)) = (slot, id, seen) else {
            continue;
        };
        let (Ok(slot), Ok(id)) = (u8::try_from(slot), u8::try_from(id)) else {
            continue;
        };
        if now - seen > SYSTEM_ID_STALE_AFTER_S {
            continue;
        }
        out.insert(
            slot,
            SlotSystemId {
                fc_system_id: id,
                conflict: false,
            },
        );
    }
    let ids: Vec<u8> = out.values().map(|s| s.fc_system_id).collect();
    for entry in out.values_mut() {
        entry.conflict = ids.iter().filter(|i| **i == entry.fc_system_id).count() > 1;
    }
    out
}

pub(super) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Expand the wire snapshot's short keys into readable ones for the sidecar.
///
/// The short names exist to fit a radio frame; nothing past the radio should
/// have to know them. An absent wire field stays absent here rather than
/// becoming a null, so "unknown" survives the translation intact.
pub(super) fn expand_status(s: &NodeStatus) -> Value {
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, v: Value| {
        if !v.is_null() {
            m.insert(k.to_string(), v);
        }
    };
    put("uptime_seconds", json!(s.up));
    put("agent_version", json!(s.ver));
    put("board_name", json!(s.bn));
    put("board_soc", json!(s.bs));
    put("board_tier", json!(s.bt));
    put("fc_connected", json!(s.fc));
    put("mavlink_alive", json!(s.fa));
    put("fc_variant", json!(s.fv));
    put("fc_firmware", json!(s.ff));
    put("services_running", json!(s.sr));
    put("services_failed", json!(s.sf));
    put("services_other", json!(s.so));
    put("failed_units", json!(s.sn));
    put("cpu_percent", json!(s.cp));
    put("memory_percent", json!(s.mp));
    put("disk_percent", json!(s.dp));
    put("temperature_c", json!(s.tc));
    put("camera_state", json!(s.cs));
    put("video_state", json!(s.vs));
    put("video_stream_count", json!(s.vn));
    Value::Object(m)
}

/// Rewrite the sidecar on a fixed cadence until cancelled.
///
/// Writes one document immediately so a reader sees a valid (empty) file before
/// the first frame, rather than an absent one it would have to treat as a fault.
pub async fn persist_loop(
    cache: AuxPeerCache,
    presence: Option<crate::presence::GsPresenceCache>,
    cancel: Shutdown,
) {
    let path = std::path::PathBuf::from(crate::paths::run_path(AUX_PEERS_SIDECAR));
    loop {
        let now = now_unix();
        let payload = cache.sidecar_payload(now);
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(&path, &payload, 0o644) {
            tracing::debug!(error = %e, "aux_peers_persist_failed");
        }
        // Hand the freshly-learned identities to the linked-peers surface, which
        // is beacon-driven and so can carry a signal reading for a peer it cannot
        // name. Pushing the CURRENT fresh set each tick (rather than accumulating)
        // means an identity whose peer went quiet drops out on its own.
        if let Some(p) = &presence {
            p.set_aux_identities(cache.fresh_identities(now));
        }
        tokio::select! {
            _ = cancel.wait() => break,
            _ = tokio::time::sleep(PERSIST_CADENCE) => {}
        }
    }
}
