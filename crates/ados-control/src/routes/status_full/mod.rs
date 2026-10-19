//! The consolidated status route: agent info, services, resources, video,
//! telemetry, radio, and mesh in one body.
//!
//! `GET /api/status/full` is the single round-trip the GCS polls instead of four
//! separate requests (`/api/status` + `/api/services` + `/api/system` +
//! `/api/video`). On this native front the agent runs as a separate daemon with
//! no in-process service tracker, no video pipeline object, and no in-process WFB
//! manager — so each block is sourced the same way the FastAPI route's
//! multi-process branch is:
//!
//! - **agent info** (`version`, `uptime_seconds`, `board`, `health`,
//!   `fc_connected`/`fc_port`/`fc_baud`) — the same seams `/api/status` reads: the
//!   version string, the state snapshot's runtime extras, the board sidecar, and
//!   the logging store's hardware snapshots with a direct host read behind them.
//!   A field neither source supplies is `null`, never `0`.
//! - **`services`** — the systemd-fallback inventory (`systemctl list-units
//!   ados-*.service`), one object per unit shaped `{name, state, sub_state,
//!   task_done, memory_mb}`. There is no per-unit transition log, so no uptime
//!   is reported (a consumer shows it as unknown rather than a fabricated 0). An
//!   inactive unit whose last start systemd skipped on an unmet unit condition
//!   reads `state: "idle"` with `reason: "condition_unmet"`, as the supervisor
//!   holds it.
//! - **`resources`** — CPU / memory / swap / disk / temperature derived from the
//!   store's most-recent hardware snapshots, the same 13-field subset the FastAPI
//!   `/api/status/full` selects from `derive_resources`. An unreachable store (or
//!   one missing an essential field) degrades to an empty object — the most-
//!   degraded shape the FastAPI route emits when both the store and psutil are
//!   unavailable.
//! - **`video`** — the multi-process branch: there is no in-process pipeline, so
//!   the block is built from a mediamtx probe (drone profile) or gated on the WFB
//!   receive link actually delivering frames (ground-station profile), with an
//!   empty recording block (no in-process recorder on this daemon).
//! - **`telemetry`** — the vehicle-state snapshot with the four runtime-only
//!   extras stripped, identical to `/api/telemetry`.
//! - **`radio`** — the forward-compatible heartbeat radio block, in the camelCase
//!   shape the GCS reads, built from the same WFB status view the video gate uses
//!   so the two can never disagree about the link.
//! - **`mesh`** — populated only on a ground-station-profile node with a non-direct
//!   role; an empty object otherwise so clients can feature-detect cheaply.
//! - **`profile`/`role`/`runtimeMode`** — the resolved wire-form profile + role
//!   discriminators and the native-vs-packaged runtime badge.
//! - **camera keys** — `cameraState` + `cameraUsbRecovery` folded in from their
//!   sidecars when fresh, so a LAN-paired operator sees the camera-missing signal
//!   the cloud heartbeat carries.
//! - **`crsf`** — the CRSF/ExpressLRS RC control-lane state, folded in from the
//!   lane's `crsf-stats.json` sidecar when it is live (mtime-fresh), the same way
//!   the `radio` block is sourced from its sidecar. PROFILE-AGNOSTIC: a drone
//!   running the ELRS relay lane surfaces it over the LOCAL-FIRST LAN path, not
//!   just a ground station — so the tab lights up for every node the lane runs
//!   on. Absent (the key omitted) when the lane is down or its sidecar stale,
//!   never a fabricated block. Served as the full snake_case sidecar field set —
//!   the richer set including `flyable` + `pic` + `fc_command_down_gated` the
//!   cloud heartbeat projection drops — so the GCS reads these keys directly.
//! - **`linked_peers` + `peerDeviceId`/`peerRssiDbm`** — the WFB peers this node
//!   currently decodes a PresenceBeacon from, folded in from the
//!   `linked-peers.json` sidecar (per-entry staleness-gated). This carries the
//!   LOCAL-FIRST transitive-enrollment signal (a ground node's relayed drones)
//!   onto the PRIMARY LAN path, so a GCS paired only to a ground node enrols each
//!   relayed drone without the cloud relay — the same signal the cloud heartbeat
//!   carries. The array entries use the snake_case shape the GCS reader consumes
//!   (`device_id` / `role` / `channel` / `rssi_dbm` / `seen_at_unix`, remapping the
//!   sidecar's `last_seen_unix`); the two scalars are the freshest-peer fallback.
//!   PROFILE-AGNOSTIC (it reads the sidecar, not the profile); the keys are absent
//!   when no fresh peer is decoded, never a fabricated block.
//!
//! Every read is fault-tolerant: an absent store / sidecar / config / systemctl
//! degrades that block to the same empty/default shape the FastAPI route returns
//! when its own source is unavailable, never a 500. The route carries no path
//! params and never mutates.

use crate::state::AppState;
use ados_protocol::wfb_status::build_radio_block;
use axum::extract::State;
use axum::Json;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

mod blocks;
mod presence;

pub(crate) use blocks::{
    base::*, camera::*, crsf::*, fc_identity::*, mesh::*, peers::*, radio::*, reconcilers::*,
    video::*,
};

// ---------------------------------------------------------------------------
// GET /api/status/full
// ---------------------------------------------------------------------------

/// `GET /api/status/full` → the consolidated status body.
///
/// Sources every block from the multi-process seams (the state snapshot, the
/// logging store, the radio sidecar, systemd, and the config), exactly as the
/// FastAPI route's no-in-process-object branch does. Guaranteed 200: each block
/// degrades in place when its source is unavailable.
pub async fn get_full_status(State(state): State<AppState>) -> Json<Value> {
    let config_profile = config_agent_profile(&state.pairing_paths.config);
    let (resolved_profile, resolved_role) =
        crate::profile::current_profile_and_role(&config_profile);

    // --- Agent info (same seams as /api/status) ---
    let snapshot = state.state.snapshot();
    let (fc_connected, fc_port, fc_baud, snapshot_uptime) =
        crate::routes::status::fc_from_snapshot(snapshot.as_ref());
    // The FC-liveness detail (read before `snapshot` is moved into the telemetry
    // projection below): the same camelCase fields /api/status carries.
    let fc_liveness = crate::routes::status::fc_liveness_from_snapshot(snapshot.as_ref());
    let uptime = snapshot_uptime.unwrap_or_else(|| json!(state.process_uptime_seconds()));
    let board = crate::routes::status::read_board(&state.board_path);

    // Hardware signals, read once and reused for health + resources.
    //
    // Store first, then a direct host read. `/api/status` has done this since the
    // zero-health bug was fixed there; this route did not, so on a stock node it
    // reported literal 0% CPU / 0% memory / 0% disk — the durable store ships
    // OFF and the installer masks its unit, so `latest_hw_signals()` is `None` on
    // every install that has not turned the key. Zeros are a legal reading, so
    // the GCS rendered a healthy idle node. The fallback is the same
    // `crate::hw_local` read the sibling route uses; when neither source answers
    // `derive_health` emits nulls rather than numbers.
    let signals = match state.logd.latest_hw_signals().await {
        Some(s) => Some(s),
        None => {
            let local = crate::hw_local::collect_signals();
            (!local.is_empty()).then_some(local)
        }
    };
    let health = crate::routes::status::derive_health(signals.as_ref());
    let resources = derive_resources_subset(signals.as_ref());

    // --- Services + sidecar folds, shared by concurrent pollers (2 s TTL) ---
    let run_dir = state.run_dir.clone();
    let slow = SLOW_BLOCKS
        .get_or_compute(SLOW_BLOCKS_TTL, || build_slow_blocks(run_dir))
        .await;
    let services = slow.get("services").cloned().unwrap_or_else(|| json!([]));

    // Router-owned posture flags, read before the snapshot moves into the
    // telemetry projection. They describe the agent itself, so they are
    // reported whether or not a flight controller is attached.
    let router_extras = router_status_extras(snapshot.as_ref());

    // --- WFB status, read once and reused by the video gate + the radio block ---
    let wfb_status = wfb_status_view(&state).await;

    // --- Video (multi-process branch: no in-process pipeline) ---
    let video = build_video_block(&resolved_profile, &wfb_status).await;

    // --- Telemetry (vehicle state minus the four runtime-only extras) ---
    let telemetry = crate::routes::status::project_telemetry(snapshot);

    // --- Radio (camelCase heartbeat block) ---
    let radio = radio_to_camel(build_radio_block(wfb_status.as_ref()));

    // --- Mesh (ground-station profile, non-direct role only) ---
    let mesh = build_mesh_block(&config_profile).await;

    // The perception capability + tier, before `board` is moved into the payload.
    // Same canonical ados_offload::pick_tier decision the LAN /api/status uses,
    // fed the same live backend-capability read, so the GCS Perception hub
    // reads a consistent tier from either route.
    let backend_inference_capable = crate::routes::status::live_backend_inference_capable().await;
    let (npu_tops, has_accelerator, perception_tier, offload_target) =
        crate::routes::status::perception_fields(&board, backend_inference_capable);

    let mut payload = Map::new();
    payload.insert("version".to_string(), json!(state.agent_version()));
    payload.insert("uptime_seconds".to_string(), uptime);
    payload.insert("board".to_string(), board);
    payload.insert("npuTops".to_string(), json!(npu_tops));
    payload.insert("hasAccelerator".to_string(), json!(has_accelerator));
    payload.insert("perceptionTier".to_string(), json!(perception_tier));
    payload.insert(
        "perceptionOffloadTarget".to_string(),
        match offload_target {
            Some(t) => Value::String(t),
            None => Value::Null,
        },
    );
    // Derived before the inserts below move `fc_liveness`'s fields out.
    let fc_reachable = super::status::derive_fc_reachable(&fc_liveness);
    let fc_variant = fc_liveness.fc_variant.clone();
    payload.insert("health".to_string(), health);
    payload.insert("fc_connected".to_string(), fc_connected);
    payload.insert("fc_port".to_string(), fc_port);
    payload.insert(
        "sim".to_string(),
        json!(ados_config::simulation::simulation_enabled()),
    );
    presence::insert_presence_flags(
        &mut payload,
        &resolved_profile,
        &presence::PresenceDirs {
            run: &state.run_dir,
            etc: state
                .pairing_paths
                .config
                .parent()
                .unwrap_or_else(|| Path::new("/etc/ados")),
            bin: &state.bin_dir,
        },
    );
    payload.insert(
        "lora".to_string(),
        json!(crate::routes::lora::lora_present(
            &state.pairing_paths.config,
            &resolved_profile
        )),
    );
    payload.insert("fc_baud".to_string(), fc_baud);
    payload.insert(
        "transportOpen".to_string(),
        json!(fc_liveness.transport_open),
    );
    payload.insert("mavlinkAlive".to_string(), json!(fc_liveness.mavlink_alive));
    payload.insert("heartbeatAgeS".to_string(), fc_liveness.heartbeat_age_s);
    payload.insert("fcSource".to_string(), fc_liveness.fc_source);
    payload.insert("fcLinkHint".to_string(), fc_liveness.fc_link_hint);
    // The honest connected-or-reachable verdict, and the protocol family it was
    // identified as. `/api/status` has carried both for a while; the full
    // payload did not, so a LAN-polling consumer had no way to tell a healthy
    // MSP flight controller (which never emits a MAVLink heartbeat, so
    // `fc_connected` stays false) from an absent one, and reported "no flight
    // controller" over a working link.
    payload.insert("fcReachable".to_string(), json!(fc_reachable));
    payload.insert("fcVariant".to_string(), fc_variant);
    // The firmware family, named the same way `/api/status` and the cloud
    // heartbeat name it, so a LAN-only node carries its ArduPilot/PX4 badge.
    payload.insert("fcFirmware".to_string(), fc_liveness.fc_firmware);
    payload.insert(
        "fcCommandDownGated".to_string(),
        json!(fc_liveness.fc_command_down_gated),
    );
    payload.insert("services".to_string(), services);
    payload.insert("resources".to_string(), resources);
    payload.insert("video".to_string(), video);
    payload.insert("telemetry".to_string(), telemetry);
    payload.insert("mesh".to_string(), mesh);
    payload.insert("radio".to_string(), radio);
    payload.insert("profile".to_string(), json!(resolved_profile));
    payload.insert(
        "role".to_string(),
        resolved_role.map(Value::from).unwrap_or(Value::Null),
    );
    payload.insert(
        "runtimeMode".to_string(),
        json!(crate::runtime_mode::current_in(
            &resolved_profile,
            &state.bin_dir
        )),
    );
    // Whether this node holds the per-pair credential its ground station's
    // relayed calls must present. Reported because the consequence is
    // operator-visible: with no secret on file every relayed request but the
    // credential delivery itself is refused, and without this field a fleet
    // operator has no way to tell an unprovisioned aircraft apart from a
    // radio fault.
    payload.insert(
        "relaySecretPresent".to_string(),
        json!(
            ados_protocol::relay_ticket::load_secret_at(&state.pairing_paths.relay_secret)
                .is_some()
        ),
    );
    // Whether the rule keeping a network-capable plugin off this node's own
    // loopback listeners is loaded. While it reads `unavailable`, a
    // `network.outbound` grant is refused and a unit holding one keeps the
    // no-network sandbox, so an operator can see why a plugin has no network.
    payload.insert(
        "plugin_loopback_guard".to_string(),
        json!(ados_protocol::plugin_loopback_guard::read_state().label()),
    );

    // The sidecar folds, computed together with the services list above:
    // camera presence + USB recovery, the reconciler verdicts (management-link
    // guardian, reach-back failover, USB rehome, WiFi power-save), per-adapter
    // stable-MAC verdicts, linked WFB peers and the CRSF lane state. Each key is
    // present only when its source is fresh, never a fabricated block.
    if let Some(Value::Object(folds)) = slow.get("folds") {
        for (k, v) in folds {
            payload.insert(k.clone(), v.clone());
        }
    }

    // The FC identity reconciler's last verdict (`/run/ados/fc-identity.json`),
    // or null before its first tick.
    payload.insert("fc_identity".to_string(), read_fc_identity(&state.run_dir));
    for (k, v) in router_extras {
        payload.insert(k, v);
    }

    // The swarm neighbour table, folded in only when `ados-swarmbus` has actually
    // published one. PROFILE-AGNOSTIC: a drone carries its own fleet view over the
    // LAN path, which is the whole point of a decentralized bus. The key is OMITTED
    // rather than emitted as `[]` when the bus is silent, because an empty array
    // would claim "this node hears no neighbours" — a different fact from "this node
    // runs no swarm bus". A client that wants the counters, or
    // the degraded shape explicitly, reads `/api/swarm/neighbors`.
    if let Some(neighbors) = super::swarm::neighbors_for_full_status(&state) {
        payload.insert("neighbors".to_string(), neighbors);
    }

    Json(Value::Object(payload))
}

/// The agent's raw `agent.profile` config value (underscore form, e.g. `"drone"`
/// / `"ground_station"` / `"auto"`), used both to resolve the wire profile and to
/// gate the mesh block on the raw `"ground_station"` value. Defaults to `"auto"`
/// (the Python `AgentConfig` default) when the file is absent.
fn config_agent_profile(config_path: &Path) -> String {
    crate::config::PairingConfig::cached(config_path)
        .agent
        .profile
        .clone()
}

// ---------------------------------------------------------------------------
// Shared slow blocks: the services inventory plus the sidecar folds.
// ---------------------------------------------------------------------------

/// How long one services + sidecar computation is reused. Every poller inside
/// the window shares one `systemctl list-units`, one `/proc` PSS scan and one
/// pass over the sidecars.
const SLOW_BLOCKS_TTL: Duration = Duration::from_secs(2);

/// The process-wide cache behind [`get_full_status`].
static SLOW_BLOCKS: TtlCache = TtlCache::new();

/// A single-flight TTL cache for one JSON value. The lock is held across the
/// computation, so concurrent callers on a cold or expired slot wait for the
/// one computation in flight instead of starting their own.
struct TtlCache {
    slot: tokio::sync::Mutex<Option<(Instant, Value)>>,
}

impl TtlCache {
    const fn new() -> Self {
        Self {
            slot: tokio::sync::Mutex::const_new(None),
        }
    }

    /// The cached value when it is younger than `ttl`, else the result of
    /// `compute`, stored for the next caller.
    async fn get_or_compute<F, Fut>(&self, ttl: Duration, compute: F) -> Value
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Value>,
    {
        let mut slot = self.slot.lock().await;
        if let Some((at, value)) = slot.as_ref() {
            if at.elapsed() < ttl {
                return value.clone();
            }
        }
        let value = compute().await;
        *slot = Some((Instant::now(), value.clone()));
        value
    }
}

/// The services list and the sidecar folds as `{"services": [..], "folds": {..}}`.
/// The sidecar reads are synchronous file IO, so they run on the blocking pool.
async fn build_slow_blocks(run_dir: PathBuf) -> Value {
    let services = build_services_list().await;
    let folds = tokio::task::spawn_blocking(move || sidecar_folds(&run_dir))
        .await
        .unwrap_or_default();
    json!({ "services": services, "folds": Value::Object(folds) })
}

/// Every sidecar-sourced key of the consolidated body, each present only when
/// its source is fresh.
fn sidecar_folds(run_dir: &Path) -> Map<String, Value> {
    let mut folds = Map::new();
    // Camera presence + USB-recovery.
    folds.extend(read_camera_status(run_dir));
    // Reconciler verdicts: management-link guardian, reach-back failover, USB
    // rehome, WiFi power-save.
    folds.extend(read_reconciler_status(run_dir));
    // Per-adapter stable-MAC verdicts, omitted on a node that pinned nothing.
    if let Some(mac_stability) =
        ados_macpin::engine::adapters_camel_from(&crate::routes::mac_adapters::state_file_path())
    {
        folds.insert("macStability".to_string(), mac_stability);
    }
    // Linked WFB peers + the freshest-peer scalars (LOCAL-FIRST transitive
    // enrollment), absent when no fresh PresenceBeacon is decoded.
    folds.extend(read_linked_peers());
    // CRSF/ExpressLRS RC control-lane state, absent when the lane is down.
    if let Some(crsf) = read_crsf_status() {
        folds.insert("crsf".to_string(), crsf);
    }
    folds
}

/// Router extras surfaced at the top level of the consolidated body: the
/// direct-GCS proxy posture (with its per-edge `*_listening` flags), the
/// unsigned-traffic rejection risk, the injector arbitration mode, and the
/// agent-held signing state. Each is null when the router has not published it.
const ROUTER_STATUS_EXTRAS: [&str; 4] = [
    "mavlink_proxy_posture",
    "agent_commands_unsigned_rejected_risk",
    "injector_arbitration",
    "agent_signing",
];

fn router_status_extras(snapshot: Option<&Value>) -> Vec<(String, Value)> {
    let obj = snapshot.and_then(Value::as_object);
    ROUTER_STATUS_EXTRAS
        .iter()
        .map(|key| {
            let value = obj
                .and_then(|m| m.get(*key))
                .cloned()
                .unwrap_or(Value::Null);
            (key.to_string(), value)
        })
        .collect()
}

#[cfg(test)]
mod tests;
