//! System routes: liveness, version/capability negotiation, and the clock probe.
//!
//! `/healthz` is the liveness probe and `/api/version` is the wire-protocol
//! version + capability flag list the GCS reads on first connect to decide which
//! features it can rely on. `/api/time` reports the wall-clock + monotonic
//! timestamps the GCS uses to estimate the drone↔browser clock offset for
//! glass-to-glass latency. The native surface must answer all three
//! byte-identically to the FastAPI surface so the same GCS works against either.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// Wire-protocol contract version. Bump when the request/response shape of any `/api/*` endpoint
/// changes in a way the GCS must adapt to. The GCS reads this and picks compatible code paths.
pub const API_VERSION: &str = "1";

/// Capability flags. Add a new flag whenever a new endpoint or behaviour ships
/// that the GCS may want to gate on. Never rename or remove a flag that once
/// worked — an older GCS may rely on the absence to take a fallback code path,
/// so a retired feature keeps its token (see `scripts.runtime` below). That
/// protection is for flags with a working past, not for a flag that never had
/// one: a claim no release ever honoured has nothing depending on it, and
/// leaving it in the list only steers a client away from the path that does
/// work. This list is the canonical surface contract between the agent and the
/// GCS; order matters, since it is emitted as a JSON array.
pub const CAPABILITIES: [&str; 15] = [
    // /api/status/full consolidated endpoint (fewer round-trips).
    "status.full",
    // /api/version endpoint (this one). Trivially true.
    "version.endpoint",
    // /api/services granular service control.
    "services.control",
    // /api/video/* live video pipeline state + transport switcher.
    "video.pipeline",
    // /api/wfb/* WFB-ng radio link control + telemetry.
    "wfb.link",
    // Retired capability. The endpoint it gated no longer ships, but the flag
    // stays in the list because this surface contract is append-only: an older
    // GCS may key a fallback path on its presence or absence, so the token is
    // never renamed or removed once shipped.
    "scripts.runtime",
    // /api/pairing/* device-link mnemonic + token rotation.
    "pairing.mnemonic",
    // /api/pairing/info carries a folded bind_state + radio snapshot.
    "pairing.bind_state",
    // /api/peripherals/* legacy hardware scan + /v1 plugin registry.
    "peripherals.registry",
    // /api/fleet/* fleet roster surface.
    "fleet.roster",
    // /api/features/* HAL feature catalog.
    "features.catalog",
    // /api/ground-station/* full ground-agent profile surface.
    "ground_station.profile",
    // /api/signing/* MAVLink v2 signing key enrollment.
    "signing.mavlink",
    // WebRTC SDP signaling broker rejection surfaced via cloud status.
    "webrtc.signaling.last_error",
    // /api/v1/battery per-pack battery health (cells, sag, time-to-reserve).
    "battery.health",
    // `can.passthrough` deliberately absent: POST /api/can/passthrough answers a
    // fixed 501 and has never opened a CAN channel, so claiming it here would
    // tell a client the bus is reachable through the agent and steer it off the
    // MAVLink CAN_FORWARD relay, which is the path that actually carries CAN
    // traffic today. The route stays registered so a probe can tell a planned
    // surface (501) from a missing one (404); the claim does not.
];

/// `GET /api/version` → `{api_version, agent_version, capabilities}`. Stable
/// shape; mirrors `version.py:get_version`.
pub async fn get_version(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "api_version": API_VERSION,
        "agent_version": state.agent_version(),
        "capabilities": CAPABILITIES,
    }))
}

/// `GET /healthz` → `{status: "ok", version}`. The liveness probe; mirrors
/// `server.py:health_check`.
pub async fn healthz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": state.agent_version(),
    }))
}

/// `GET /api/time` → `{time_ns, monotonic_ns, ntp_synced}`. The GCS browser uses
/// Cristian's algorithm against this to estimate the drone↔browser clock offset.
/// Mirrors `system.py:get_time`: a wall-clock nanosecond stamp, a monotonic
/// nanosecond counter, and the best-effort NTP-synced flag. These are live clock
/// reads, so the values change per call; the contract is the shape + types +
/// the `ntp_synced` semantics. This route is NOT in the auth-exempt set.
pub async fn get_time() -> Json<Value> {
    // The sync probe spawns processes; it runs first so the clock stamps are
    // taken just before the reply leaves, keeping the one-way delay the offset
    // estimate assumes symmetric as short as it can be.
    let synced = ntp_synced().await;
    Json(json!({
        "time_ns": wall_clock_ns(),
        "monotonic_ns": monotonic_ns(),
        "ntp_synced": synced,
    }))
}

/// `GET /api/ping` → `{pong: <server_epoch_ms>}`. A cheap, FC-independent
/// control-plane echo: the GCS times the request round-trip around its own poll
/// to measure transport RTT to the agent (the `controlRttMs` it surfaces next to
/// the link badge). The body carries the agent's wall-clock millisecond stamp so
/// a caller can also estimate one-way offset. Public (no key) so RTT can be
/// measured before a key is held; never touches the FC or any service, so it is
/// always 200 and adds no load. Distinct from `/api/time` (which reports
/// nanosecond + monotonic stamps for glass-to-glass clock-offset estimation).
pub async fn get_ping() -> Json<Value> {
    Json(json!({ "pong": wall_clock_ms() }))
}

/// Wall-clock time in milliseconds since the Unix epoch (the `pong` stamp). A
/// clock before the epoch clamps to zero rather than panicking.
fn wall_clock_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Wall-clock time in nanoseconds since the Unix epoch, matching Python
/// `time.time_ns()`. A clock before the epoch (never expected on a sane host)
/// clamps to zero rather than panicking.
fn wall_clock_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// A monotonic nanosecond counter, matching Python `time.monotonic_ns()`
/// semantics: never goes backwards, used only for delta/offset estimation. On
/// Linux this reads `CLOCK_MONOTONIC` for parity with the Python source; on a
/// non-Linux dev host it derives a monotonic count from a process-static base
/// `Instant`, which is monotonic and non-negative (the absolute value is not part
/// of the contract — the GCS uses only deltas).
#[cfg(target_os = "linux")]
fn monotonic_ns() -> u128 {
    let mut ts = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safety: `clock_gettime` writes into the provided timespec; the pointer is
    // valid for the duration of the call. CLOCK_MONOTONIC is always available on
    // Linux. A non-zero return leaves the timespec zeroed, which still yields a
    // valid (if degraded) monotonic-style value.
    let rc = unsafe { nix::libc::clock_gettime(nix::libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u128) * 1_000_000_000 + (ts.tv_nsec as u128)
}

#[cfg(not(target_os = "linux"))]
fn monotonic_ns() -> u128 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = BASE.get_or_init(Instant::now);
    base.elapsed().as_nanos()
}

/// Best-effort NTP-synced flag, mirroring `system.py:_is_ntp_synced`. On Linux it
/// probes chrony, then `timedatectl`, then the systemd-timesyncd marker file,
/// failing closed to `false`. Off Linux there is no such daemon convention, so it
/// is `false` (the dev-host / off-rig answer).
///
/// Both command probes go through [`crate::probe::capture`], which bounds them
/// and folds an absent binary into `Unavailable`. That is why there is no
/// `which` pre-flight any more: this is the highest-frequency shell-out in the
/// crate (the GCS polls `/api/time` continuously to run Cristian's algorithm),
/// and a `which` gate meant spawning a process to decide whether to spawn a
/// process. An absent `chronyc` now falls through on the probe itself, which is
/// the same outcome the gate produced.
#[cfg(target_os = "linux")]
async fn ntp_synced() -> bool {
    use std::path::Path;

    // chrony (preferred): the tracking row must name a reference and not report
    // the unsynchronised leap status. chronyc prints a full row, and exits zero,
    // even with no source at all.
    let chrony =
        crate::probe::capture("chronyc", &["-c", "tracking"], crate::probe::PROBE_TIMEOUT).await;
    if chrony.is_ok() && chrony_tracking_synced(chrony.text()) {
        return true;
    }

    // timedatectl fallback: NTPSynchronized=yes. The blocking version read
    // stdout without checking the exit status; `capture` yields text only on a
    // zero exit. `timedatectl show -p <prop> --value` exits zero whenever it
    // prints a value, so the only case that changes is a non-zero exit that
    // still printed `yes` — which then falls through to the marker file below
    // rather than short-circuiting to `true`.
    let synced = crate::probe::capture(
        "timedatectl",
        &["show", "-p", "NTPSynchronized", "--value"],
        crate::probe::PROBE_TIMEOUT,
    )
    .await;
    if synced.text().trim().eq_ignore_ascii_case("yes") {
        return true;
    }

    // systemd-timesyncd marker file: written once a reference is acquired. Left
    // as a synchronous `stat` — it is one lookup on tmpfs, so handing it to the
    // blocking pool would cost more than it saves.
    Path::new("/run/systemd/timesync/synchronized").is_file()
}

#[cfg(not(target_os = "linux"))]
async fn ntp_synced() -> bool {
    false
}

/// Whether a `chronyc -c tracking` CSV row shows a synchronised clock: a
/// reference id other than `00000000` and a leap status (the last field) other
/// than `Not synchronised`. An empty or short row is not synchronised.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn chrony_tracking_synced(row: &str) -> bool {
    let fields: Vec<&str> = row.trim().split(',').collect();
    if fields.len() < 2 {
        return false;
    }
    let ref_id = fields[0].trim();
    let leap = fields[fields.len() - 1].trim();
    !ref_id.is_empty() && ref_id != "00000000" && !leap.eq_ignore_ascii_case("not synchronised")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// A flag in [`CAPABILITIES`] is a claim that the agent can do the thing, so
    /// it may not outlive the handler that backs it. The CAN passthrough route
    /// has never opened a CAN channel — it answers a fixed `501` — and a client
    /// that reads the claim and skips the MAVLink `CAN_FORWARD` path it would
    /// otherwise take is left with no working route to the bus at all. Assert
    /// both halves together so whoever lands a real bridge has to flip the
    /// handler and the advertisement in one change rather than drifting them.
    #[tokio::test]
    async fn can_passthrough_is_unadvertised_while_its_route_is_unimplemented() {
        let unimplemented =
            crate::routes::can::can_passthrough().await.status() == StatusCode::NOT_IMPLEMENTED;
        let advertised = CAPABILITIES.contains(&"can.passthrough");
        assert!(
            !(unimplemented && advertised),
            "POST /api/can/passthrough answers 501, so `can.passthrough` must not \
             appear in the advertised capability list"
        );
    }

    /// The GCS keeps the same list as `AGENT_CAPABILITIES_FROZEN` in its
    /// cross-repo contract test (`tests/contract/agent-version-contract.test.ts`
    /// in Mission Control). A flag added, removed or reordered here must change
    /// there in the same release, so the served list is pinned whole.
    #[test]
    fn served_capabilities_match_the_gcs_contract() {
        const AGENT_CAPABILITIES_FROZEN: [&str; 15] = [
            "status.full",
            "version.endpoint",
            "services.control",
            "video.pipeline",
            "wfb.link",
            "scripts.runtime",
            "pairing.mnemonic",
            "pairing.bind_state",
            "peripherals.registry",
            "fleet.roster",
            "features.catalog",
            "ground_station.profile",
            "signing.mavlink",
            "webrtc.signaling.last_error",
            "battery.health",
        ];
        assert_eq!(
            CAPABILITIES, AGENT_CAPABILITIES_FROZEN,
            "agent contract drift: update AGENT_CAPABILITIES_FROZEN on both sides"
        );
    }

    #[test]
    fn chrony_with_no_reference_is_not_synchronised() {
        let unsynced = "00000000,,0,0.000000000,0.000000000,0.000000000,0.000000000,0.000,0.000,0.000,1.000000000,1.000000000,0.0,Not synchronised";
        assert!(!chrony_tracking_synced(unsynced));
        let synced = "A29FC87B,time.example.com,3,1700000000.123456789,-0.000012345,0.000001234,0.000045678,-12.345,0.001,0.012,0.012345678,0.001234567,1024.5,Normal";
        assert!(chrony_tracking_synced(synced));
        assert!(!chrony_tracking_synced(""));
    }

    #[test]
    fn wall_clock_ns_is_after_the_epoch() {
        // Any sane host is well past 2020 (~1.6e18 ns since the epoch).
        assert!(wall_clock_ns() > 1_600_000_000_000_000_000);
    }

    #[test]
    fn monotonic_ns_never_goes_backwards() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(b >= a, "monotonic clock went backwards: {a} -> {b}");
    }

    #[tokio::test]
    async fn ntp_synced_returns_a_bool_without_panicking() {
        // Off Linux this is false; on Linux it is a best-effort probe. Either way
        // the call must not panic.
        let _ = ntp_synced().await;
    }

    #[test]
    fn wall_clock_ms_is_after_the_epoch() {
        // Any sane host is well past 2020 (~1.6e12 ms since the epoch).
        assert!(wall_clock_ms() > 1_600_000_000_000);
    }

    #[tokio::test]
    async fn ping_returns_a_pong_millisecond_stamp() {
        let Json(body) = get_ping().await;
        let pong = body.get("pong").and_then(|v| v.as_u64());
        assert!(
            pong.is_some_and(|v| v > 1_600_000_000_000),
            "pong: {body:?}"
        );
    }
}
