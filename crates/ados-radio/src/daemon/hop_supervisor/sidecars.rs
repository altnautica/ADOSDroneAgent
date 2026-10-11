//! Contract E hop/peer sidecar writers and the hop-supervisor body.

use super::*;

/// Latch a freshly-learned peer device-id for back-fill into the persisted pair
/// state. Writes `peer-backfill.json` = `{"peer_device_id": <id>}` atomically.
///
/// The reader is `ados-control`'s `routes::peer_backfill` reconciler, which owns
/// the config.yaml round-trip so the radio service never clobbers unrelated
/// config keys (every other write to that file goes through the front too). The
/// bind tunnel does not always carry the peer id, so the presence beacon is the
/// canonical source.
pub(super) fn write_peer_backfill_json(peer_device_id: &str) {
    let v = json!({ "peer_device_id": peer_device_id });
    let _ = write_sidecar(&run_path("peer-backfill.json"), &v);
}

/// Write `peer-presence.json` (Contract E) from the shared hop state.
pub(super) async fn write_peer_presence_json(state: &Arc<tokio::sync::Mutex<HopState>>) {
    let v = {
        let s = state.lock().await;
        match s.peer() {
            Some(p) => json!({
                "peer_device_id": p.device_id,
                "peer_role": p.role,
                "peer_channel": p.channel,
                "peer_rssi_dbm": p.rssi_dbm,
                "peer_last_seen_unix": s.peer_last_seen_unix(),
            }),
            None => json!({
                "peer_device_id": serde_json::Value::Null,
                "peer_role": serde_json::Value::Null,
                "peer_channel": serde_json::Value::Null,
                "peer_rssi_dbm": serde_json::Value::Null,
                "peer_last_seen_unix": serde_json::Value::Null,
            }),
        }
    };
    let _ = write_sidecar(&run_path("peer-presence.json"), &v);
}

/// Write `hop-supervisor.json` (Contract E) from the shared hop state + config.
/// `enabled_channels` is the regulatory-permitted channel set used to intersect
/// hop candidates; surfacing it lets the panel show why a hop was refused.
/// The honest reason the time-based periodic hop is not executing, or `None`
/// when it is active. Reported in the hop sidecar so the panel never shows
/// `enabled:true` for a path that cannot fire (the prior sidecar always reported
/// `auto_hop_enabled`, which masked the suppressed periodic path).
pub(crate) fn periodic_hop_suppression_reason(cfg: &WfbConfig) -> Option<&'static str> {
    if !cfg.auto_hop_enabled {
        Some("auto_hop_disabled")
    } else if !cfg.periodic_hop_enabled {
        Some("periodic_hop_opt_in_off")
    } else {
        None
    }
}

pub(super) async fn write_hop_supervisor_json(
    state: &Arc<tokio::sync::Mutex<HopState>>,
    cfg: &WfbConfig,
    enabled_channels: &std::collections::BTreeSet<u8>,
    periodic_hop_enabled: bool,
) {
    let v = {
        let s = state.lock().await;
        hop_supervisor_body(
            &s,
            cfg,
            enabled_channels,
            periodic_hop_enabled,
            periodic_hop_suppression_reason(cfg),
        )
    };
    let _ = write_sidecar(&run_path("hop-supervisor.json"), &v);
}

/// Publish that hopping is refused because the shared key file exists but
/// cannot be read. Nothing hops and no hop frame is authenticated until the
/// key reads cleanly again (the radio re-reads it on its next generation).
pub(crate) fn write_hop_key_unavailable(cfg: &WfbConfig) {
    let state = HopState::new(cfg.rendezvous_channel());
    let v = hop_supervisor_body(
        &state,
        cfg,
        &std::collections::BTreeSet::new(),
        false,
        Some("key_unavailable"),
    );
    let _ = write_sidecar(&run_path("hop-supervisor.json"), &v);
}

pub(super) fn hop_supervisor_body(
    s: &HopState,
    cfg: &WfbConfig,
    enabled_channels: &std::collections::BTreeSet<u8>,
    periodic_hop_enabled: bool,
    suppression: Option<&'static str>,
) -> serde_json::Value {
    let history =
        serde_json::to_value(s.history()).unwrap_or_else(|_| serde_json::Value::Array(vec![]));
    let enabled: Vec<u8> = enabled_channels.iter().copied().collect();
    json!({
        // Sidecar schema version (best-effort drift signal for readers).
        // Shared with the ground-station hop persister via the one const.
        "version": crate::paths::HOP_SUPERVISOR_SIDECAR_VERSION,
        // `enabled` is the honest periodic-execution state, not the bare
        // `auto_hop_enabled`: a suppressed periodic path reports false plus a
        // reason. The reactive hop + the GS-coordinated follow run regardless.
        "enabled": periodic_hop_enabled,
        "auto_hop_enabled": cfg.auto_hop_enabled,
        "periodic_hop_enabled": cfg.periodic_hop_enabled,
        "periodic_suppression_reason": suppression,
        "band": cfg.band,
        "hop_period_seconds": cfg.hop_period_seconds,
        "loss_threshold_percent": cfg.hop_loss_threshold_percent as f64,
        "rssi_threshold_dbm": cfg.hop_rssi_threshold_dbm as f64,
        "enabled_channels": enabled,
        "last_hop_at": s.last_hop_at_unix(),
        "history": history,
        "wall_time_unix": crate::hop::now_unix(),
    })
}
