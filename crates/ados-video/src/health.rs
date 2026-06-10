//! Pure health-decision logic for the video-pipeline FSM.
//!
//! Every function here is a side-effect-free transition over a sampled value
//! (a counter, an elapsed time, a restart attempt count), so each one is
//! unit-testable without a single subprocess. The orchestrator's health tick
//! reads the live counters, applies these decisions, and then performs the
//! side effects (latching first-packet, recording bytes/s, restarting). The
//! sequencing is a faithful port of the Python `VideoPipeline` health rules.

use std::time::{Duration, Instant};

// --- tunables (mirror constants.py + pipeline.py) ----------------------------

/// Health-tick cadence (`_HEALTH_CHECK_INTERVAL`).
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Max startup grace before a publisher-less pipeline is declared dead
/// (`_STARTUP_GRACE_MAX_SECS`).
pub const STARTUP_GRACE_MAX: Duration = Duration::from_secs(30);
/// Inbound-byte stall window (`_INBOUND_FLOW_STALL_SECONDS`).
pub const INBOUND_FLOW_STALL: Duration = Duration::from_secs(12);
/// Base restart delay (`_base_restart_delay`).
pub const BASE_RESTART_DELAY: Duration = Duration::from_secs(5);
/// Cap on the exponential restart backoff for a real wedge (`_max_restart_delay`).
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(300);
/// Tighter cap when the failure is "no primary camera" — a USB hotplug
/// condition that resolves in seconds (`_max_restart_delay_no_camera`).
pub const MAX_RESTART_DELAY_NO_CAMERA: Duration = Duration::from_secs(30);
/// Consecutive-healthy window that clears the restart counter
/// (`_healthy_reset_window_secs`).
pub const HEALTHY_RESET_WINDOW: Duration = Duration::from_secs(60);
/// Ceiling on the wfb-tee restart backoff (the Python `min(..., 5.0)`).
pub const WFB_TEE_RESTART_CEILING: Duration = Duration::from_secs(5);
/// Consecutive-failure count that trips the 5-minute circuit-breaker park.
pub const CIRCUIT_BREAKER_ATTEMPTS: u32 = 10;

/// Pipeline lifecycle state (`PipelineState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    Stopped,
    Starting,
    Running,
    Error,
}

/// The tagged cause of the most recent `start_stream` failure, so the retry
/// loop can pick the right backoff cap (`_last_start_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// No camera won the auto-assign — transient USB hotplug; 30 s cap.
    NoPrimaryCamera,
    /// No encoder backend available for the camera.
    NoEncoder,
    /// The encoder subprocess failed to spawn.
    EncoderSpawnFailed,
    /// mediamtx failed to start.
    MediamtxFailed,
    /// The last start succeeded or the cause is unknown — 5-minute cap.
    None,
}

// --- pure health-decision functions (testable without subprocesses) ----------

/// Exponential backoff with a cap, in the Python `min(base * 2^(n-1), cap)`
/// shape. `attempt` is 1-based.
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    // 2^(attempt-1), saturating so a large attempt count cannot overflow.
    let shift = attempt - 1;
    let factor: u64 = 1u64.checked_shl(shift.min(63)).unwrap_or(u64::MAX);
    let scaled = base
        .as_secs_f64()
        .mul_add(factor as f64, 0.0)
        .min(cap.as_secs_f64());
    Duration::from_secs_f64(scaled)
}

/// Pick the backoff cap for the error-state retry: the no-camera cap when the
/// last failure was a missing primary, otherwise the full 5-minute cap.
pub fn retry_cap(last_error: StartError) -> Duration {
    match last_error {
        StartError::NoPrimaryCamera => MAX_RESTART_DELAY_NO_CAMERA,
        _ => MAX_RESTART_DELAY,
    }
}

/// Should the circuit breaker trip (park for 5 minutes and reset the counter)?
pub fn circuit_breaker_tripped(restart_count: u32) -> bool {
    restart_count >= CIRCUIT_BREAKER_ATTEMPTS
}

/// Should a sustained-healthy run clear the restart counter? True once the
/// pipeline has been continuously healthy for strictly longer than
/// [`HEALTHY_RESET_WINDOW`] (the Python `> window` comparison).
pub fn healthy_window_elapsed(healthy_since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(healthy_since) > HEALTHY_RESET_WINDOW
}

/// The decision the startup-grace branch of the health check makes, given the
/// mediamtx publisher probe + elapsed time. Pure so it is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceDecision {
    /// A publisher appeared — latch first-packet and report healthy.
    FirstPacket,
    /// Still inside the grace window with no publisher yet — report healthy.
    StillWaiting,
    /// Grace expired with no publisher — report unhealthy (restart).
    Expired,
}

/// Grace-window decision (`_check_health` pre-first-packet block).
pub fn grace_decision(path_ready: bool, elapsed: Duration) -> GraceDecision {
    if path_ready {
        GraceDecision::FirstPacket
    } else if elapsed < STARTUP_GRACE_MAX {
        GraceDecision::StillWaiting
    } else {
        GraceDecision::Expired
    }
}

/// The inbound-byte watchdog decision (`_check_inbound_flow_healthy`), as a
/// pure transition over the prior counter sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InboundDecision {
    /// First sample — seed the counter, report healthy.
    Seed,
    /// Counter advanced — report healthy, record the new bytes/s.
    Advanced { bytes_per_s: f64 },
    /// Counter flat but still within the stall window — report healthy.
    WithinStall,
    /// Counter flat past the stall window — report unhealthy (restart publish).
    Stalled,
}

/// Inbound-flow decision over a new `current` byte sample.
///
/// `prev` is the previously recorded counter (`< 0` ⇒ no sample yet).
/// `since_change` is how long the counter has sat flat. `interval` floors the
/// elapsed used for the bytes/s rate so a zero-elapsed sample cannot divide by
/// zero (mirrors the Python `max(now - changed_at, interval)`).
pub fn inbound_decision(
    prev: i64,
    current: i64,
    since_change: Duration,
    interval: Duration,
) -> InboundDecision {
    if prev < 0 {
        return InboundDecision::Seed;
    }
    if current > prev {
        let delta = (current - prev) as f64;
        let elapsed = since_change.max(interval).as_secs_f64();
        return InboundDecision::Advanced {
            bytes_per_s: delta / elapsed,
        };
    }
    if since_change < INBOUND_FLOW_STALL {
        InboundDecision::WithinStall
    } else {
        InboundDecision::Stalled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_ladder_matches_python_shape() {
        // base 5s, cap 300s → 5,10,20,40,80,160,300(capped),300,...
        assert_eq!(
            backoff_delay(1, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(2, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(10)
        );
        assert_eq!(
            backoff_delay(3, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(20)
        );
        assert_eq!(
            backoff_delay(4, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(40)
        );
        assert_eq!(
            backoff_delay(5, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(80)
        );
        assert_eq!(
            backoff_delay(6, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(160)
        );
        // 7 → 320 capped to 300.
        assert_eq!(
            backoff_delay(7, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(300)
        );
        assert_eq!(
            backoff_delay(20, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::from_secs(300)
        );
        // attempt 0 is a no-op (defensive).
        assert_eq!(
            backoff_delay(0, BASE_RESTART_DELAY, MAX_RESTART_DELAY),
            Duration::ZERO
        );
    }

    #[test]
    fn no_camera_cap_is_30s() {
        // base 5s, cap 30s → 5,10,20,30(capped),30,...
        assert_eq!(
            backoff_delay(1, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(2, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(10)
        );
        assert_eq!(
            backoff_delay(3, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(20)
        );
        // 4 → 40 capped to 30.
        assert_eq!(
            backoff_delay(4, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(30)
        );
        assert_eq!(
            backoff_delay(10, BASE_RESTART_DELAY, MAX_RESTART_DELAY_NO_CAMERA),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn wfb_tee_ceiling_is_5s() {
        // wfb tee backoff caps at 5s regardless of attempt.
        assert_eq!(
            backoff_delay(1, BASE_RESTART_DELAY, WFB_TEE_RESTART_CEILING),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(2, BASE_RESTART_DELAY, WFB_TEE_RESTART_CEILING),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay(5, BASE_RESTART_DELAY, WFB_TEE_RESTART_CEILING),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn retry_cap_picks_by_error_class() {
        assert_eq!(
            retry_cap(StartError::NoPrimaryCamera),
            MAX_RESTART_DELAY_NO_CAMERA
        );
        assert_eq!(retry_cap(StartError::NoEncoder), MAX_RESTART_DELAY);
        assert_eq!(retry_cap(StartError::EncoderSpawnFailed), MAX_RESTART_DELAY);
        assert_eq!(retry_cap(StartError::MediamtxFailed), MAX_RESTART_DELAY);
        assert_eq!(retry_cap(StartError::None), MAX_RESTART_DELAY);
    }

    #[test]
    fn circuit_breaker_trips_at_ten() {
        assert!(!circuit_breaker_tripped(9));
        assert!(circuit_breaker_tripped(10));
        assert!(circuit_breaker_tripped(11));
    }

    #[test]
    fn healthy_window_boundary_at_60s() {
        let base = Instant::now();
        // Strict `>` (the Python `now - last > window`): exactly 60s is NOT
        // elapsed; just past 60s is.
        assert!(!healthy_window_elapsed(
            base,
            base + Duration::from_millis(59_999)
        ));
        assert!(!healthy_window_elapsed(
            base,
            base + Duration::from_secs(60)
        ));
        assert!(healthy_window_elapsed(
            base,
            base + Duration::from_millis(60_001)
        ));
        assert!(healthy_window_elapsed(
            base,
            base + Duration::from_secs(120)
        ));
    }

    #[test]
    fn grace_decision_transitions() {
        // Publisher present at any time → first packet.
        assert_eq!(
            grace_decision(true, Duration::ZERO),
            GraceDecision::FirstPacket
        );
        assert_eq!(
            grace_decision(true, Duration::from_secs(40)),
            GraceDecision::FirstPacket
        );
        // No publisher inside the window → still waiting.
        assert_eq!(
            grace_decision(false, Duration::from_secs(5)),
            GraceDecision::StillWaiting
        );
        assert_eq!(
            grace_decision(false, Duration::from_millis(29_999)),
            GraceDecision::StillWaiting
        );
        // No publisher past the window → expired.
        assert_eq!(
            grace_decision(false, Duration::from_secs(30)),
            GraceDecision::Expired
        );
        assert_eq!(
            grace_decision(false, Duration::from_secs(45)),
            GraceDecision::Expired
        );
    }

    #[test]
    fn inbound_decision_seed_on_first_sample() {
        assert_eq!(
            inbound_decision(-1, 1000, Duration::ZERO, HEALTH_CHECK_INTERVAL),
            InboundDecision::Seed
        );
    }

    #[test]
    fn inbound_decision_advanced_computes_rate() {
        // 10000 bytes over a 5s floored interval → 2000 B/s.
        let d = inbound_decision(0, 10_000, Duration::from_secs(5), HEALTH_CHECK_INTERVAL);
        match d {
            InboundDecision::Advanced { bytes_per_s } => {
                assert!((bytes_per_s - 2000.0).abs() < 1e-6, "got {bytes_per_s}");
            }
            other => panic!("expected Advanced, got {other:?}"),
        }
        // A sub-interval elapsed is floored to the interval so the rate cannot
        // spike artificially.
        let d = inbound_decision(0, 5_000, Duration::from_millis(100), HEALTH_CHECK_INTERVAL);
        match d {
            InboundDecision::Advanced { bytes_per_s } => {
                // floored to 5s → 1000 B/s, not 50000 B/s.
                assert!((bytes_per_s - 1000.0).abs() < 1e-6, "got {bytes_per_s}");
            }
            other => panic!("expected Advanced, got {other:?}"),
        }
    }

    #[test]
    fn inbound_decision_within_and_past_stall() {
        // Flat counter inside the 12s stall window → still healthy.
        assert_eq!(
            inbound_decision(1000, 1000, Duration::from_secs(11), HEALTH_CHECK_INTERVAL),
            InboundDecision::WithinStall
        );
        // Flat counter at exactly 12s → stalled.
        assert_eq!(
            inbound_decision(1000, 1000, Duration::from_secs(12), HEALTH_CHECK_INTERVAL),
            InboundDecision::Stalled
        );
        // Counter went backwards (mediamtx path reset) is treated as flat →
        // stall logic applies.
        assert_eq!(
            inbound_decision(2000, 1500, Duration::from_secs(20), HEALTH_CHECK_INTERVAL),
            InboundDecision::Stalled
        );
    }
}
