//! Cloud status heartbeat loop.
//!
//! Every 5 s, when paired, POST the frozen [`HeartbeatPayload`] to
//! `{convex}/agent/status` with `X-ADOS-Key` auth. Originally ported from the
//! Python heartbeat loop, which has since been deleted.
//!
//! ## Native enrichment
//!
//! The Python payload folds in psutil/systemctl/board enrichment: CPU/mem/disk
//! samples, per-service status, the radio block, LCD/display fields, CAN buses,
//! etc. The live status the GCS needs (resources + FC link + service fleet) is
//! built natively in Rust by the [`crate::loops::enrichment`] producer each tick
//! and folded over the deterministic native base here via [`build_payload`].
//!
//! The base itself ([`HeartbeatBase`] → [`native_payload`]) carries only the
//! fields the loop always has without probing (device identity, version, board),
//! with every enrichment field left `Option` + skip-if-none on the frozen
//! payload. So even if the enrichment producer returns nothing, the loop emits a
//! valid heartbeat with the required fields (`deviceId`/`version`/
//! `uptimeSeconds`) and absence reads as honest "unknown" rather than a
//! fabricated `0` / `false` / `"stopped"`. The wire
//! `HeartbeatPayload` (frozen + golden-tested) stays byte-identical.

use std::time::Duration;

use super::sidecars::*;
use crate::heartbeat::{HeartbeatPayload, RadioBlock, RemoteAccess};

/// Heartbeat cadence: a 5 s base sleep, as the receiver expects.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// The top-level keys whose receiver column is declared NULL-TOLERANT
/// (`v.optional(v.union(T, v.null()))`) and which this loop's producers are
/// responsible for. An explicit JSON `null` on one of these means "reported,
/// not measured", and the null-strip step below must leave it alone.
///
/// Why it matters: the receiver builds its patch from its own declared column
/// list, and Convex drops `undefined`-valued keys during argument
/// serialization, so a key ABSENT from the body reads as "this agent does not
/// report this at all" — indistinguishable from an agent that predates the
/// surface, and nothing the receiver can distinguish from a deliberate
/// no-reading. An explicit null is the one form that says "I report this and
/// have no reading", and it CLEARS the column. The `capability_extras` producer
/// already emits exactly that (see its `or_null`) for the reach-back and
/// USB-rehome fields; the blanket null-strip that used to run here deleted
/// those nulls off the wire, so a guardian that went from "failing over on
/// wlan0" to "nothing to report" left `wlan0` standing in the cloud row.
///
/// Everything NOT on this list is `v.optional(T)` on the receiver, which
/// rejects an explicit null outright, so those keys must still be stripped.
pub const UNMEASURED_NULL_KEYS: &[&str] = &[
    // The hoisted WFB adapter verdicts. `null` = no radio view, so no scan and
    // no chipset; `false` on the injection key is a MEASURED scanned-none.
    "wfbAdapterChipset",
    "wfbAdapterInjectionOk",
    // The management-link guardian's reach-back failover detail, and the
    // USB-rehome self-heal outcome. Their producer distinguishes "no value" from
    // "no surface" and emits null for the former.
    "mgmtFailoverIface",
    "mgmtFailoverReason",
    "usbRehomeLastResult",
];

/// The deterministic, native inputs the loop always has without any probing:
/// the device identity, version, profile, and uptime. Everything else is
/// enrichment folded from the sidecar.
#[derive(Debug, Clone)]
pub struct HeartbeatBase {
    pub device_id: String,
    pub version: String,
    pub profile: Option<String>,
    pub role: Option<String>,
    pub uptime_seconds: i64,
    pub board_name: String,
    pub board_tier: i64,
    pub board_soc: String,
    pub board_arch: String,
    /// NPU throughput (TOPS) from the board sidecar; 0 when the board has no NPU.
    /// Feeds the perception-tier decision on the cloud beacon.
    pub board_npu_tops: f64,
    /// The board profile declares CPU-ONNX local inference (an NPU-less but
    /// CPU-strong board runs the detector on-board). Feeds the perception-tier
    /// decision alongside `board_npu_tops`; false on an older board sidecar.
    pub board_local_inference: bool,
}

/// Build the heartbeat wire object from the native base plus an optional
/// enrichment JSON object. The enrichment is the Python sidecar's contents
/// (already in the frozen wire shape — camelCase root keys, snake_case `radio`);
/// its keys are folded OVER the native base, then the required base fields
/// (`deviceId` / `version` / `uptimeSeconds`) are re-asserted so a producer can
/// never drop or diverge them, and the top level is null-stripped (Convex
/// `v.optional` rejects an explicit null) EXCEPT for the declared
/// [`UNMEASURED_NULL_KEYS`], whose null is a meaningful "not measured". A
/// `None` enrichment yields the native-only object with an all-`absent` radio
/// block and no optional fields.
///
/// Returns a `serde_json::Value` (the POST body): the producer owns the full
/// payload shape, and operating at the value level keeps the merge faithful
/// without forcing every frozen field to be deserialize-tolerant. The frozen
/// [`HeartbeatPayload`] is the source of the native base object's shape +
/// casing. This is the testable per-tick assembly; the loop calls it then POSTs.
pub fn build_payload(
    base: &HeartbeatBase,
    enrichment: Option<&serde_json::Value>,
) -> serde_json::Value {
    // The native base, in the frozen wire shape (camelCase root, snake_case
    // radio, required fields set, optionals absent).
    let native = native_payload(base).to_value();
    let mut obj = native.as_object().cloned().unwrap_or_default();

    // Fold the capability sidecars the LAN `/api/status/full` route already
    // folds — camera presence + USB recovery, the management-link guardian,
    // reach-back mode, USB rehome, Wi-Fi power-save, the LCD page. Without
    // them a cloud-relayed node was a second-class node in the GCS: the same
    // hardware, four empty cards and a `capability:camera` miss on a drone
    // with a working camera, purely because of which transport the reading
    // arrived over. Folded BEFORE the enrichment, so a producer that observes
    // one of these live still wins.
    for (k, v) in crate::loops::capabilities::capability_extras() {
        obj.insert(k, v);
    }

    // Fold the enrichment keys over the base.
    if let Some(serde_json::Value::Object(map)) = enrichment {
        for (k, v) in map {
            obj.insert(k.clone(), v.clone());
        }
    }

    // Re-assert the required base identity so a producer cannot drop or diverge
    // it (the wire contract demands these three).
    obj.insert("deviceId".to_string(), serde_json::json!(base.device_id));
    obj.insert("version".to_string(), serde_json::json!(base.version));
    obj.insert(
        "uptimeSeconds".to_string(),
        serde_json::json!(base.uptime_seconds),
    );

    // Null-strip the top level: most receiver columns are `v.optional(T)`, which
    // accepts absent-or-T but not an explicit null. The declared null-tolerant
    // keys are EXEMPT — for them a null is the "reported, not measured" reading
    // that clears the column, and stripping it silently left the previous tick's
    // value standing in the cloud row forever (see [`UNMEASURED_NULL_KEYS`]).
    // The nested `radio` object keeps its own nulls either way (the strip is
    // top-level only, matching the contract).
    obj.retain(|k, v| !v.is_null() || UNMEASURED_NULL_KEYS.contains(&k.as_str()));
    serde_json::Value::Object(obj)
}

/// A payload carrying only the required + native fields, an all-`absent` radio
/// block, and no optional enrichment.
fn native_payload(base: &HeartbeatBase) -> HeartbeatPayload {
    let video_streams = read_video_streams_sidecar(now_epoch_ms());
    // Fold the WFB linked-peers list (None on a drone / a peerless ground node,
    // and None when every entry is stale so a dead listener is not folded
    // forever). A ground station relaying drones surfaces `linkedPeers[]` here.
    let linked_peers = read_linked_peers_sidecar(now_epoch_ms());
    // Fold the CRSF RC-lane state (None on a node without the lane, and None
    // when the file is stale so a dead lane service is not folded forever).
    let crsf = read_crsf_sidecar();
    // Ferry every fresh plugin/feature state slice opaquely (empty map omitted).
    let plugin_state = {
        let slices = read_plugin_state_sidecars();
        if slices.is_empty() {
            None
        } else {
            Some(slices)
        }
    };
    // Surface any LIVE service config-parse fault from the config-status sidecars
    // (empty ⇒ omitted, so a healthy node's wire is unchanged).
    let config_errors = {
        let errs = read_config_error_sidecars();
        if errs.is_empty() {
            None
        } else {
            Some(errs)
        }
    };
    // The perception tier this node runs on: the canonical ados_offload::pick_tier
    // decision from the board's local compute path (an NPU, or the profile-declared
    // CPU-ONNX local inference) + the live offload-link the reconciler writes (a
    // paired, reachable workstation flips compute_node_paired + bearer_acceptable
    // true and names the target). Absent / stale ⇒ no link ⇒ a board with a local
    // path reads `local` and one without any path reads `none`; the offload target
    // stays absent until a workstation is actually paired. Fed
    // identically to /api/status via `TierInputs::for_drone`.
    let has_accelerator = base.board_npu_tops > 0.0;
    let offload_link = ados_protocol::offload_link::read_offload_link(now_epoch_ms());
    let (link_paired, link_bearer_ok) = offload_link
        .as_ref()
        .map(|l| (l.paired, l.bearer_acceptable))
        .unwrap_or((false, false));
    let perception_tier = match ados_offload::pick_tier(&ados_offload::TierInputs::for_drone(
        has_accelerator,
        base.board_local_inference,
        link_paired,
        link_bearer_ok,
    )) {
        Some(ados_offload::PerceptionTier::Local) => "local",
        Some(ados_offload::PerceptionTier::Offload) => "offload",
        Some(ados_offload::PerceptionTier::Hybrid) => "hybrid",
        None => "none",
    }
    .to_string();
    // Surface the target only on an actual offload path.
    let perception_offload_target = offload_link
        .filter(|l| l.is_offload_path())
        .and_then(|l| l.target);

    HeartbeatPayload {
        device_id: base.device_id.clone(),
        version: base.version.clone(),
        profile: base.profile.clone(),
        role: base.role.clone(),
        uptime_seconds: base.uptime_seconds,
        board_name: base.board_name.clone(),
        board_tier: base.board_tier,
        board_soc: base.board_soc.clone(),
        board_arch: base.board_arch.clone(),
        npu_tops: base.board_npu_tops,
        has_accelerator,
        perception_tier,
        perception_offload_target,
        // Unmeasured by the native loop: omitted (None) so the wire says
        // "unknown" rather than asserting a 0 / false / "stopped" reading the
        // loop never took. The enrichment loop
        // folds the real values over these absences each tick.
        cpu_percent: None,
        memory_percent: None,
        disk_percent: None,
        temperature: None,
        memory_used_mb: None,
        memory_total_mb: None,
        disk_used_gb: None,
        disk_total_gb: None,
        cpu_cores: None,
        board_ram_mb: None,
        cpu_history: None,
        memory_history: None,
        fc_connected: None,
        fc_port: None,
        fc_baud: None,
        // The FC link gated-truth detail is the enrichment producer's to lift from
        // the state snapshot; the native base leaves it absent (honest "unknown").
        transport_open: None,
        mavlink_alive: None,
        heartbeat_age_s: None,
        fc_source: None,
        fc_link_hint: None,
        // The FC variant is the enrichment producer's to lift from the state
        // snapshot; the native base leaves it absent (honest "unknown").
        fc_variant: None,
        services: None,
        // The node's advertised reach. These used to be sent as `""` every tick,
        // which is NOT "unknown" on the receiver — `stringField` type-checks an
        // empty string straight into the column, so each 5 s heartbeat destroyed
        // a previously-good `lastIp`/`mdnsHost`/`setupUrl`/`apiUrl` and the GCS
        // then built `http://:8080` out of it.
        //
        // `mdnsHost` is the one reach this loop can PROVE: the canonical
        // resolver reads the system hostname avahi actually answers on and
        // returns None when the host has no name that could be another
        // machine's reach. The IP-derived three are proven by nothing here (no
        // producer in this crate resolves the node's primary address or checks
        // that :8080 accepts), so they are omitted. Advertise only a reach name
        // proven to resolve; an unproven one is worse than none.
        last_ip: None,
        mdns_host: ados_protocol::reach::mdns_hostname(),
        setup_url: None,
        api_url: None,
        agent_version: base.version.clone(),
        video_state: None,
        video_whep_port: None,
        mavlink_ws_port: None,
        mavlink_ws_url: None,
        video_whep_url: None,
        mission_control_url: None,
        remote_access: RemoteAccess {
            provider: "none".to_string(),
            public_urls: vec![],
        },
        last_plugin_update_check_at: None,
        peripherals: None,
        // The live block when the radio's stats sidecar is fresh, else the honest
        // absent skeleton. This was hardcoded absent, so a cloud-relayed node
        // rendered an empty link card forever while the same data sat on disk and
        // the LAN routes read it fine.
        radio: read_radio_sidecar().unwrap_or_else(RadioBlock::absent),
        crsf,
        wfb_adapter_chipset: None,
        // The per-adapter stable-MAC verdicts, from the same projection the LAN
        // status body uses. Six GCS readers and three cards waited on this key
        // and no transport ever produced it.
        mac_stability: ados_macpin::engine::adapters_camel_from(std::path::Path::new(
            &std::env::var("ADOS_MAC_PINS_STATE")
                .unwrap_or_else(|_| ados_macpin::engine::STATE_PATH.to_string()),
        )),
        // No radio view in the native base ⇒ no injection verdict: the key is
        // omitted (never a fabricated false) until the radio enrichment folds
        // a real scan outcome over the base.
        wfb_adapter_injection_ok: None,
        lcd_active_page: None,
        ui_theme: None,
        lcd_touch_calibrated: None,
        lcd_rotation: None,
        lcd_snapshot_url: None,
        lcd_last_touch_at: None,
        lcd_last_gesture: None,
        video_local_decoder_active: None,
        video_local_decoder_type: None,
        video_local_decoder_fps: None,
        video_recording: None,
        video_streams,
        linked_peers,
        display_type: None,
        can_buses: None,
        plugin_state,
        config_errors,
    }
}

/// What one `/agent/status` POST got back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// Answered, with this HTTP status (2xx or not).
    Answered(u16),
    /// No answer: the transport error.
    Unanswered(String),
}

/// POST one heartbeat to `{convex}/agent/status` with `X-ADOS-Key`. Never
/// fatal: the outcome is logged and returned for the cloud-link sidecar.
pub async fn post_heartbeat(
    client: &reqwest::Client,
    convex_url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> HeartbeatOutcome {
    let url = format!("{}/agent/status", convex_url.trim_end_matches('/'));
    match client
        .post(&url)
        .header("X-ADOS-Key", api_key)
        .json(body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if resp.status().is_success() {
                tracing::debug!("cloud status sent");
            } else {
                tracing::warn!(status, "cloud status rejected");
            }
            HeartbeatOutcome::Answered(status)
        }
        Err(e) => {
            tracing::debug!(error = %e, "cloud heartbeat failed");
            HeartbeatOutcome::Unanswered(e.to_string())
        }
    }
}

/// Folds heartbeat outcomes into the cloud-link sidecar record.
#[derive(Debug, Default)]
pub struct CloudLinkTracker {
    last_ok_ms: Option<i64>,
    last_status: Option<u16>,
    last_error: Option<String>,
}

impl CloudLinkTracker {
    /// Record one POST outcome observed at `now_ms`.
    pub fn record(&mut self, outcome: &HeartbeatOutcome, now_ms: i64) {
        match outcome {
            HeartbeatOutcome::Answered(status) => {
                self.last_status = Some(*status);
                self.last_error = None;
                if (200..300).contains(status) {
                    self.last_ok_ms = Some(now_ms);
                }
            }
            HeartbeatOutcome::Unanswered(e) => {
                self.last_status = None;
                self.last_error = Some(e.clone());
            }
        }
    }

    /// The sidecar record for this tick.
    pub fn link(
        &self,
        paired: bool,
        cloud_url_set: bool,
        broker_connected: Option<bool>,
        now_ms: i64,
    ) -> ados_protocol::cloud_link::CloudLink {
        ados_protocol::cloud_link::CloudLink {
            version: ados_protocol::cloud_link::CLOUD_LINK_SIDECAR_VERSION,
            generated_at_ms: Some(now_ms),
            paired,
            cloud_url_set,
            broker_connected,
            last_heartbeat_ok_ms: self.last_ok_ms,
            last_heartbeat_status: self.last_status,
            last_heartbeat_error: self.last_error.clone(),
        }
    }
}

#[cfg(test)]
mod tests;
