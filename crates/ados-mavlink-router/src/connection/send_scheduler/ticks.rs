//! The periodic stream cadence, banner request and parameter sweep ticks.

use super::*;
use crate::connection::imu_fallback::{MSG_HIGHRES_IMU, MSG_RAW_IMU, RAW_IMU_FALLBACK_HZ};

impl FcConnection {
    /// Adaptive stream request. Picks the interval from how long the link has
    /// been idle (stalled link re-requests fast; healthy link relaxes toward
    /// the max), then re-sends the per-message rates when the interval elapses.
    ///
    /// `#[allow(deprecated)]`: the `legacy_stream_request` branch below builds a
    /// spec-superseded `REQUEST_DATA_STREAM` on purpose; the branch comment says
    /// why.
    #[allow(deprecated)]
    pub async fn tick_streams(&self) {
        if !self.transport_open() {
            return;
        }
        let idle = self.last_msg_idle();
        {
            let mut interval = self.stream_interval.lock().await;
            *interval = if idle >= STREAM_STALL {
                STREAM_MIN
            } else if idle <= STREAM_HEALTHY {
                (*interval + STREAM_STEP).min(STREAM_MAX)
            } else {
                *interval
            };
        }
        let interval = *self.stream_interval.lock().await;
        // The table follows the profile and the capture marker; within the
        // capture table HIGHRES_IMU gives way to RAW_IMU when the FC never
        // answered it. A change of either re-requests at once.
        let now = Instant::now();
        let rates = stream_rates(&self.cfg.profile, self.sensor_streams.capture_active());
        let capture_table = rates.iter().any(|&(id, _)| id == MSG_HIGHRES_IMU);
        let raw = capture_table && self.sensor_streams.poll(now);
        let changed = self.sensor_streams.changed((capture_table, raw));
        let due = changed || {
            let last = self.last_stream_req.lock().await;
            last.map(|t| t.elapsed() >= interval).unwrap_or(true)
        };
        if !due {
            return;
        }
        let target = self.target_system.load(Ordering::Relaxed);
        for &(msg_id, rate_hz) in rates {
            let (msg_id, rate_hz) = if raw && msg_id == MSG_HIGHRES_IMU {
                (MSG_RAW_IMU, RAW_IMU_FALLBACK_HZ)
            } else {
                (msg_id, rate_hz)
            };
            let interval_us = 1_000_000.0 / rate_hz;
            let cmd = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
                target_system: target,
                target_component: 1,
                command: MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL,
                confirmation: 0,
                param1: msg_id as f32,
                param2: interval_us,
                param3: 0.0,
                param4: 0.0,
                param5: 0.0,
                param6: 0.0,
                param7: 0.0,
            });
            self.send_msg(&cmd).await;
        }
        self.sensor_streams.requested(capture_table, raw, now);
        // The legacy `REQUEST_DATA_STREAM` groups, off by default.
        //
        // This used to be sent unconditionally, on the assumption that a firmware
        // honors either the interval requests above or the legacy groups but never
        // both. Measured MAVLink ingest on ArduPilot was 66.5 frames/s against the
        // 29 Hz the interval requests sum to — consistent with ArduPilot honoring
        // BOTH paths and streaming roughly twice the telemetry that was asked for,
        // on a radio link whose airtime is the binding constraint for a fleet.
        // Default-off therefore halves the request traffic and the ingest it
        // provokes.
        //
        // The flag, not a deletion, because the legacy path is the ONLY one iNav /
        // Betaflight / pre-4.1 ArduPilot honor: if a firmware turns out to answer
        // only `REQUEST_DATA_STREAM`, its ingest collapses instead of halving, and
        // setting `mavlink.legacy_stream_request: true` is the rollback.
        if self.cfg.legacy_stream_request {
            for &(stream_id, rate_hz) in STREAM_GROUPS {
                let req = MavMessage::REQUEST_DATA_STREAM(REQUEST_DATA_STREAM_DATA {
                    target_system: target,
                    target_component: 1,
                    req_stream_id: stream_id,
                    req_message_rate: rate_hz,
                    start_stop: 1,
                });
                self.send_msg(&req).await;
            }
        }
        *self.last_stream_req.lock().await = Some(Instant::now());
    }

    /// Until ArduPilot's banner has named its firmware, ask for it every
    /// [`BANNER_RETRY`]. A warm parameter cache sends no PARAM_REQUEST_LIST
    /// (the other thing that provokes the banner), and until it arrives a
    /// copter-typed heartbeat cannot be told from a QuadPlane, so no flight
    /// mode can be named or commanded. Its own fixed cadence rather than the
    /// stream refresh's, which relaxes to tens of seconds on a healthy link and
    /// left a freshly connected copter unidentified for that long.
    pub async fn tick_banner_request(&self) {
        if !self.transport_open() {
            return;
        }
        let unidentified = {
            let s = self.state.lock().await;
            s.firmware.is_none()
                && s.autopilot == MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA as i64
                && !s.last_heartbeat.is_empty()
        };
        if !unidentified {
            return;
        }
        {
            let mut last = self.last_banner_req.lock().await;
            if last.is_some_and(|t| t.elapsed() < BANNER_RETRY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let cmd = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
            target_system: self.target_system.load(Ordering::Relaxed),
            target_component: 1,
            command: MavCmd::MAV_CMD_DO_SEND_BANNER,
            confirmation: 0,
            param1: 0.0,
            param2: 0.0,
            param3: 0.0,
            param4: 0.0,
            param5: 0.0,
            param6: 0.0,
            param7: 0.0,
        });
        self.send_msg(&cmd).await;
    }

    /// Parameter sweep with the priming/timeout flags the GCS spinner reads.
    /// Rate-limited to one PARAM_REQUEST_LIST per [`PARAM_RATE_LIMIT`]; flips
    /// the timeout flag when the deadline passes with no parameters cached.
    pub async fn tick_param_sweep(&self) {
        if !self.transport_open() {
            return;
        }
        // Progress check: clear priming once the cache is fully populated.
        let cached = self.params.lock().await.count();
        let expected = self.state.lock().await.param_count.max(0) as usize;
        if expected > 0 && cached >= expected {
            self.param_priming.store(false, Ordering::Relaxed);
            self.param_sweep_timed_out.store(false, Ordering::Relaxed);
            return;
        }
        let due = {
            let last = self.param_last_request.lock().await;
            last.map(|t| t.elapsed() >= PARAM_RATE_LIMIT)
                .unwrap_or(true)
        };
        if !due {
            // While priming and past the deadline with nothing cached, flag the timeout.
            if self.param_priming.load(Ordering::Relaxed) && cached == 0 {
                if let Some(started) = *self.param_sweep_started.lock().await {
                    if started.elapsed() >= PARAM_DEADLINE {
                        self.param_sweep_timed_out.store(true, Ordering::Relaxed);
                    }
                }
            }
            return;
        }

        // `due` only means PARAM_RATE_LIMIT has elapsed since our own last
        // sweep send — it says nothing about whether the download actually
        // needs help. Compare the cache size now against its size the last
        // time we checked: if it grew at all in that window, something (our
        // own prior sweep response still landing, or a downstream client's
        // own PARAM_REQUEST_LIST) is genuinely still making progress, and
        // injecting a competing PARAM_REQUEST_LIST here would livelock it —
        // the FC tracks queued-parameter state PER LINK, not per requester,
        // so a second request on this link restarts its enumeration from
        // index 0, undoing exactly the progress that is happening. Only fire
        // once the cache has genuinely stalled across a full rate-limit
        // window, which is what this housekeeping sweep exists to recover
        // from in the first place.
        let last_cached = self.param_last_cached_count.swap(cached, Ordering::Relaxed);
        if cached > last_cached {
            return;
        }

        let target = self.target_system.load(Ordering::Relaxed);
        let req = MavMessage::PARAM_REQUEST_LIST(PARAM_REQUEST_LIST_DATA {
            target_system: target,
            target_component: 1,
        });
        let ok = self.send_msg(&req).await;
        self.param_sweep_send_failed.store(!ok, Ordering::Relaxed);
        self.param_priming.store(true, Ordering::Relaxed);
        let now = Instant::now();
        *self.param_last_request.lock().await = Some(now);
        let mut started = self.param_sweep_started.lock().await;
        if started.is_none() {
            *started = Some(now);
        }
    }
}
