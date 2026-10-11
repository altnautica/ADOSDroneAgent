//! Per-generation helpers of the direct receive loop: the 1 Hz receive-link
//! telemetry task and the in-generation fleet reconcile.

use super::*;

/// 1 Hz receive-link telemetry for one generation: ship the link's RSSI / SNR /
/// uncorrected-FEC (the uplink command radio, mirroring the drone-side downlink
/// video radio) and a lock/unlock event on a real link-state transition.
/// Aborted with the generation. Best-effort; an absent logging daemon drops the
/// samples without disturbing receive.
pub(super) fn spawn_link_telemetry(
    ingest: &ados_protocol::logd::emitter::IngestEmitter,
    link: &Arc<Mutex<LinkStats>>,
) -> tokio::task::JoinHandle<()> {
    let emitter = ingest.clone();
    let link = link.clone();
    tokio::spawn(async move {
        use ados_protocol::logd::{Fields, Level, Value};
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut prev_locked: Option<bool> = None;
        loop {
            tick.tick().await;
            let stats = link.lock().await.clone();
            let rx_key_present = std::path::Path::new(RX_KEY).exists();
            // A ground station is a receive end: it never injects the
            // video plane, so `tx_live` is false and the unverified
            // branch is structurally unreachable here. It proves the
            // link by its own decodes, which the stats above carry.
            let state = ados_radio::link_state::derive_link_state(
                rx_key_present,
                false,
                &stats,
                false,
                false,
            );
            let mut tags = Fields::new();
            tags.insert("direction".to_string(), Value::from("uplink"));
            tags.insert("link".to_string(), Value::from("command"));
            emitter.emit_metric("link.rssi_dbm", stats.rssi_dbm, tags.clone());
            emitter.emit_metric("link.snr_db", stats.snr_db, tags.clone());
            emitter.emit_metric("link.fec_uncorrected", stats.fec_failed as f64, tags);
            let locked = state.is_locked();
            if prev_locked != Some(locked) {
                let mut detail = Fields::new();
                detail.insert("link".to_string(), Value::from("command"));
                detail.insert("state".to_string(), Value::from(state.as_str()));
                if locked {
                    emitter.emit_event("link.lock", Level::Info, detail);
                } else if prev_locked.is_some() {
                    emitter.emit_event("link.unlock", Level::Warn, detail);
                }
                prev_locked = Some(locked);
            }
        }
    })
}

/// Re-read the fleet registry on a slow tick and add/remove the SECONDARY
/// slots' receivers in place, and bring each secondary's video receiver up or
/// down as the hero selection moves. Returns (ending the generation) only when
/// the PRIMARY slot is released.
pub(super) async fn reconcile_fleet(
    manager: &WfbRxManager,
    interface: &str,
    primary_slot: u8,
    secondaries: &mut std::collections::BTreeMap<u8, SlotReceivers>,
    progress: &ados_supervisor::sdnotify::MonitorProgress,
) {
    let mut tick = tokio::time::interval(FLEET_RECONCILE_INTERVAL);
    tick.tick().await; // the first tick completes immediately
    let mut hero_tick = tokio::time::interval(Duration::from_secs(1));
    hero_tick.tick().await;
    let hero_path = std::path::PathBuf::from(crate::fleet_hero::hero_path());
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let want: std::collections::BTreeSet<u8> = wfb_rx::fleet_slots(
                    &FleetRegistry::load(std::path::Path::new(FLEET_REGISTRY_PATH)),
                )
                .into_iter()
                .collect();
                if !want.contains(&primary_slot) {
                    tracing::info!(primary_slot, "ground_primary_slot_released");
                    return;
                }
                secondaries.retain(|slot, _| {
                    let keep = want.contains(slot);
                    if !keep {
                        tracing::info!(slot, "ground_slot_rx_despawned");
                    }
                    keep
                });
                // Collect the missing slots before spawning: the filter
                // borrows `secondaries` and the insert needs it mutably.
                let missing: Vec<u8> = want
                    .iter()
                    .copied()
                    .filter(|s| *s != primary_slot && !secondaries.contains_key(s))
                    .collect();
                for slot in missing {
                    match manager.spawn_slot_receivers(interface, slot, false).await {
                        Ok(r) => {
                            secondaries.insert(slot, r);
                            tracing::info!(slot, "ground_slot_rx_spawned");
                        }
                        Err(e) => {
                            tracing::error!(
                                error = %e, slot, "ground_slot_rx_failed_to_start"
                            )
                        }
                    }
                }
            }
            // Video bring-back, driven off the 1 s hero poll: only the
            // hero slot's video egress is read, so a secondary slot
            // decodes a full-FEC video stream nobody consumes. Bring the
            // video receiver up when the slot BECOMES the hero and drop
            // it (killpg) when it is unpromoted. `aux` + `control` were
            // spawned unconditionally and stay up either way.
            _ = hero_tick.tick() => {
                // The steady-state progress stamp. A healthy generation
                // can hold the select for hours, so without a stamp from
                // inside it the watchdog would conclude the loop had
                // wedged and restart a perfectly good receive plane.
                progress.mark();
                let hero = fanout::resolve_fanout_slot(
                    primary_slot,
                    &hero_path,
                    std::path::Path::new(FLEET_REGISTRY_PATH),
                );
                for (slot, rx) in secondaries.iter_mut() {
                    let is_hero = *slot == hero;
                    if is_hero && !rx.decoding_video() {
                        match manager.spawn_slot_video(interface, *slot).await {
                            Ok(v) => {
                                rx.video = Some(v);
                                tracing::info!(slot, "ground_slot_video_brought_up");
                            }
                            Err(e) => tracing::error!(
                                error = %e, slot, "ground_slot_video_failed_to_start"
                            ),
                        }
                    } else if !is_hero && rx.decoding_video() {
                        // Dropping the receiver killpg's its group.
                        rx.video = None;
                        tracing::info!(slot, "ground_slot_video_unwatched");
                    }
                }
            }
        }
    }
}
