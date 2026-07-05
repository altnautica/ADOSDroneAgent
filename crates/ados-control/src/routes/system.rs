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

/// Wire-protocol contract version. Bump when the request/response shape of any
/// `/api/*` endpoint changes in a way the GCS must adapt to. The GCS reads this
/// and picks compatible code paths. Mirrors the Python `API_VERSION`.
pub const API_VERSION: &str = "1";

/// Capability flags. Add a new flag whenever a new endpoint or behaviour ships
/// that the GCS may want to gate on. Never rename or remove a flag once shipped
/// — an older GCS may rely on the absence to take a fallback code path. This
/// list is the canonical surface contract between the agent and the GCS, kept in
/// lock-step with the Python `CAPABILITIES` list (order included, since it is
/// emitted as a JSON array).
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
    // /api/can/passthrough route presence. Today the route returns 501; the flag
    // lets the GCS detect whether the surface exists at all so it can fall back
    // to MAVLink CAN_FORWARD without probing.
    "can.passthrough",
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
    Json(json!({
        "time_ns": wall_clock_ns(),
        "monotonic_ns": monotonic_ns(),
        "ntp_synced": ntp_synced(),
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
#[cfg(target_os = "linux")]
fn ntp_synced() -> bool {
    use std::path::Path;
    use std::process::Command;

    // chrony (preferred): a successful `chronyc -c tracking` with a non-empty row
    // means chrony has a reference.
    if which_on_path("chronyc") {
        if let Ok(out) = Command::new("chronyc").args(["-c", "tracking"]).output() {
            if out.status.success() && !out.stdout.is_empty() {
                let s = String::from_utf8_lossy(&out.stdout);
                if !s.trim().is_empty() {
                    return true;
                }
            }
        }
    }

    // timedatectl fallback: NTPSynchronized=yes.
    if which_on_path("timedatectl") {
        if let Ok(out) = Command::new("timedatectl")
            .args(["show", "-p", "NTPSynchronized", "--value"])
            .output()
        {
            if String::from_utf8_lossy(&out.stdout)
                .trim()
                .eq_ignore_ascii_case("yes")
            {
                return true;
            }
        }
    }

    // systemd-timesyncd marker file: written once a reference is acquired.
    Path::new("/run/systemd/timesync/synchronized").is_file()
}

#[cfg(not(target_os = "linux"))]
fn ntp_synced() -> bool {
    false
}

/// True when `name` is found as an executable on `PATH`. A bare-command lookup
/// matching `shutil.which`, used to gate the chrony / timedatectl probes so a
/// fresh rootfs without either tool fails closed. Linux-only (the call sites are
/// Linux-gated).
#[cfg(target_os = "linux")]
fn which_on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        let candidate = dir.join(name);
        std::fs::metadata(&candidate)
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn ntp_synced_returns_a_bool_without_panicking() {
        // Off Linux this is false; on Linux it is a best-effort probe. Either way
        // the call must not panic.
        let _ = ntp_synced();
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
