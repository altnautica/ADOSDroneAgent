//! The scenario matrix. Each scenario builds a [`World`] for a seed, drives
//! its faults at seeded times, steps it to the end and checks the end state
//! (the per-tick invariants run throughout, in [`crate::checks`]).

use std::future::Future;

use ados_mavlink_router::config::MavlinkConfig;

use crate::chan::Faults;
use crate::downlink::PilotSide;
use crate::fc;
use crate::gcs::Outcome;
use crate::sim::World;

pub type Fly = fn(u64) -> World;

pub const ALL: &[(&str, Fly)] = &[
    ("fc_link", fc_link),
    ("mission_mirror", mission_mirror),
    ("pilot_downlink", pilot_downlink),
    ("gate_crsf", gate_crsf),
    ("gate_visor", gate_visor),
];

/// Run `f` on a one-thread Tokio runtime whose clock is paused: it moves
/// only when the run steps it, so steady time is simulated time too.
fn block(f: impl Future<Output = World>) -> World {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a runtime")
        .block_on(f)
}

/// A serial FC link with seeded loss, duplication and jitter.
fn serial(w: &mut World) -> Faults {
    Faults {
        loss: w.rng.range(0.01, 0.05),
        dup: 0.01,
        delay_ms: (2, 25),
    }
}

fn drone() -> MavlinkConfig {
    MavlinkConfig::default()
}

fn end<T>(w: &World, r: Result<T, String>) -> T {
    r.unwrap_or_else(|e| w.fail(format_args!("end state: {e}")))
}

/// The FC link: quick reboots inside the heartbeat timeout, a long outage,
/// a radio blackout with the FC still up, a USB replug that reboots it.
fn fc_link(seed: u64) -> World {
    block(async move {
        let mut w = World::new("fc_link", seed, drone(), Faults::CLEAN).await;
        let f = serial(&mut w);
        w.up.faults = f;
        w.down.faults = f;
        let jitter = |w: &mut World| w.rng.between(0, 8_000);

        let t = 30_000 + jitter(&mut w);
        w.run_to(t).await;
        let off = w.rng.between(1_500, 3_500);
        w.note(format_args!("fc quick reboot {off} ms"));
        w.fc.power_off(t + off);
        // The re-request after the reboot loses its ATTITUDE frame: one
        // stream missing while the rest flow.
        w.run_to(t + off + 500).await;
        w.drop_request = Some(30);

        let t = 70_000 + jitter(&mut w);
        w.run_to(t).await;
        let off = w.rng.between(6_000, 12_000);
        w.note(format_args!("fc outage {off} ms"));
        w.fc.power_off(t + off);

        let t = 110_000 + jitter(&mut w);
        w.run_to(t).await;
        let dark = w.rng.between(5_000, 9_000);
        w.note(format_args!("radio blackout {dark} ms"));
        w.up.down = true;
        w.down.down = true;
        w.run_to(t + dark).await;
        w.up.down = false;
        w.down.down = false;

        let t = 150_000 + jitter(&mut w);
        w.run_to(t).await;
        w.unplug(t + 3_000).await;
        w.run_to(t + 3_000).await;
        w.replug().await;

        let t = 190_000 + jitter(&mut w);
        w.run_to(t).await;
        let off = w.rng.between(1_000, 3_000);
        w.note(format_args!("fc quick reboot {off} ms"));
        w.fc.power_off(t + off);

        // A clean tail: a request the lossy link dropped is re-sent by the
        // healthy link's refresh (60 s at most), so the streams must end up
        // flowing and the requests sparse.
        w.run_to(240_000).await;
        w.up.faults = Faults::CLEAN;
        w.down.faults = Faults::CLEAN;
        w.run_to(320_000).await;
        let now = w.now;
        if !w.live.alive || !w.live.att_fresh(now) || !w.fc.streaming(30) {
            w.fail(format_args!(
                "end state: the link did not recover its streams"
            ));
        }
        let late = w.live.bursts.iter().filter(|t| **t > now - 100_000).count();
        if late > 3 {
            w.fail(format_args!(
                "end state: {late} stream request bursts in the last 100 s of a healthy link"
            ));
        }
        let unsupported = w.router.fc.fc_streams_unsupported();
        if unsupported.is_empty() || unsupported.iter().any(|id| fc::STREAMABLE.contains(id)) {
            w.fail(format_args!(
                "end state: streams reported unsupported: {unsupported:?}"
            ));
        }
        w
    })
}

/// Ground-station uploads over lossy links while the mirror downloads, a
/// router restart and an FC reboot; the mirror ends holding the FC's plan.
fn mission_mirror(seed: u64) -> World {
    block(async move {
        let mut w = World::new("mission_mirror", seed, drone(), Faults::CLEAN).await;
        let f = serial(&mut w);
        w.up.faults = f;
        w.down.faults = f;
        let air = Faults {
            loss: w.rng.range(0.03, 0.08),
            dup: 0.02,
            delay_ms: (5, 60),
        };
        w.with_gcs(air, 0.0);
        for (i, at) in [20_000u64, 70_000, 140_000, 200_000]
            .into_iter()
            .enumerate()
        {
            let t = at + w.rng.between(0, 10_000);
            w.run_to(t).await;
            // The third keeps the FC's item count: only the ACK tells the
            // mirror the plan changed.
            let n = if i == 2 {
                w.fc.mission.len().max(2) as u16 - 1
            } else {
                w.rng.between(1, 40) as u16
            };
            let items = fc::random_mission(&mut w.rng, n);
            w.note(format_args!("gcs upload {} items", items.len()));
            let now = w.now;
            w.gcs.as_mut().expect("present").gcs.upload(now, items);
            if i == 1 {
                let t = w.now + w.rng.between(30_000, 40_000);
                w.run_to(t).await;
                w.restart_router().await;
            }
            if i == 2 {
                let t = w.now + w.rng.between(20_000, 40_000);
                w.run_to(t).await;
                let off = w.rng.between(1_500, 4_000);
                w.note(format_args!("fc reboot {off} ms"));
                w.fc.power_off(t + off);
            }
        }
        // Another ground station on another link changes the plan while the
        // router's own uplink is dead: MISSION_CURRENT's total says the
        // mirror is stale, and its download fails until the uplink returns.
        let t = 228_000 + w.rng.between(0, 4_000);
        w.run_to(t).await;
        let n = w.fc.mission.len() as u16 + w.rng.between(1, 5) as u16;
        w.fc.mission = fc::random_mission(&mut w.rng, n);
        let dead = w.rng.between(12_000, 16_000);
        w.note(format_args!(
            "plan changed out of band, uplink dead {dead} ms"
        ));
        w.up.down = true;
        w.run_to(t + dead).await;
        w.up.down = false;
        w.run_to(260_000).await;
        // A clean tail: what the mirror converges to once the links settle.
        w.up.faults = Faults::CLEAN;
        w.down.faults = Faults::CLEAN;
        let g = w.gcs.as_mut().expect("present");
        g.to_router.faults = Faults::CLEAN;
        g.to_gcs.faults = Faults::CLEAN;
        w.run_to(320_000).await;
        let r = w.mirror_matches().await;
        end(&w, r);
        let g = w.gcs.as_ref().expect("present");
        if !g
            .gcs
            .uploads
            .iter()
            .any(|u| u.outcome.is_some_and(|(_, o)| o == Outcome::Accepted))
        {
            w.fail(format_args!("end state: no upload was accepted"));
        }
        if w.router.mirror.downloads_ok == 0 {
            w.fail(format_args!(
                "end state: the mirror never completed a download"
            ));
        }
        w
    })
}

/// The pilot's state downlink: states of every size, the aux radio
/// refusing for a while, the pilot restarting, a lossy air link.
fn pilot_downlink(seed: u64) -> World {
    block(async move {
        let mut w = World::new("pilot_downlink", seed, drone(), Faults::CLEAN).await;
        let f = serial(&mut w);
        w.up.faults = f;
        w.down.faults = f;
        let air = Faults {
            loss: w.rng.range(0.02, 0.08),
            dup: 0.02,
            delay_ms: (5, 80),
        };
        w.pilot = Some(PilotSide::new(w.router.fc.clone(), air, w.now));
        for at in [25_000u64, 75_000] {
            let t = at + w.rng.between(0, 10_000);
            w.run_to(t).await;
            let d = w.rng.between(2_000, 5_000);
            w.note(format_args!("aux radio refuses {d} ms"));
            w.pilot
                .as_mut()
                .expect("present")
                .radio
                .lock()
                .unwrap()
                .refuse = true;
            w.run_to(t + d).await;
            w.pilot
                .as_mut()
                .expect("present")
                .radio
                .lock()
                .unwrap()
                .refuse = false;

            let t = w.now + w.rng.between(5_000, 15_000);
            w.run_to(t).await;
            let d = w.rng.between(1_000, 3_000);
            w.note(format_args!("pilot restart {d} ms"));
            w.pilot.as_mut().expect("present").silent_until = t + d;
        }
        w.run_to(130_000).await;
        let p = w.pilot.as_mut().expect("present");
        p.silent_until = u64::MAX;
        w.run_to(131_000).await;
        let p = w.pilot.as_ref().expect("present");
        let (sent, failed, oversize) = p.counters();
        let air = &p.air;
        if sent + failed + oversize != p.due || sent != air.offered {
            w.fail(format_args!(
                "end state: {} due states, {sent} sent + {failed} failed + {oversize} oversize, {} on the air",
                p.due, air.offered
            ));
        }
        if p.delivered != air.offered - air.lost + air.duplicated {
            w.fail(format_args!(
                "end state: the ground got {} of {} sent",
                p.delivered, air.offered
            ));
        }
        if failed == 0 || oversize == 0 {
            w.fail(format_args!(
                "end state: no send failure or oversize state was exercised"
            ));
        }
        w
    })
}

/// A MAVLink-over-ELRS source with its command direction gated: telemetry
/// flows, nothing goes up, and the mirror reports it cannot download.
fn gate_crsf(seed: u64) -> World {
    block(async move {
        let cfg = MavlinkConfig {
            crsf_enabled: true,
            crsf_mode: "mavlink".into(),
            crsf_device: "/dev/ttyUSB0".into(),
            ..MavlinkConfig::default()
        };
        let mut w = World::new("gate_crsf", seed, cfg, Faults::CLEAN).await;
        let f = serial(&mut w);
        w.down.faults = f;
        for (id, ms) in [(30, 100), (1, 500), (42, 500)] {
            w.fc.stream_by_default(id, ms);
        }
        let air = Faults {
            loss: 0.03,
            dup: 0.02,
            delay_ms: (5, 40),
        };
        w.with_gcs(air, 0.01);
        let t = 20_000 + w.rng.between(0, 5_000);
        w.run_to(t).await;
        let items = fc::random_mission(&mut w.rng, 8);
        let now = w.now;
        w.gcs.as_mut().expect("present").gcs.upload(now, items);
        w.run_to(90_000).await;
        let g = w.gcs.as_ref().expect("present");
        if g.accepted != 0 || g.flight_sent == 0 || g.benign_sent == 0 {
            w.fail(format_args!(
                "end state: {} client writes accepted",
                g.accepted
            ));
        }
        if g.gcs
            .uploads
            .iter()
            .any(|u| !matches!(u.outcome, Some((_, Outcome::TimedOut))))
        {
            w.fail(format_args!(
                "end state: an upload over a gated link did not time out"
            ));
        }
        let mission = w.router.mission_json().await;
        let stale = mission.get("route_stale").and_then(|v| v.as_bool()) == Some(true);
        if !w.live.alive
            || w.router.mirror.downloads_failed == 0
            || !stale
            || w.router.mirror.mirror.present
        {
            w.fail(format_args!(
                "end state: telemetry lost, or a mirror that could not download did not say so"
            ));
        }
        w
    })
}

/// A visor: its FC is a sensor hub, so a ground station's flight commands
/// never reach it while everything else does.
fn gate_visor(seed: u64) -> World {
    block(async move {
        let cfg = MavlinkConfig {
            profile: "visor".into(),
            ..MavlinkConfig::default()
        };
        let mut w = World::new("gate_visor", seed, cfg, Faults::CLEAN).await;
        let f = serial(&mut w);
        w.up.faults = f;
        w.down.faults = f;
        // The capture table falls back from HIGHRES_IMU to RAW_IMU, which
        // re-requests at once by design.
        w.check_stream_spacing = false;
        let air = Faults {
            loss: 0.03,
            dup: 0.02,
            delay_ms: (5, 40),
        };
        w.with_gcs(air, 0.1);
        w.run_to(90_000).await;
        let g = w.gcs.as_ref().expect("present");
        if g.flight_sent == 0 || g.heard[0] == 0 || w.router.fc.visor_flight_frames_dropped() == 0 {
            w.fail(format_args!(
                "end state: {} flight commands sent, {} benign heard, {} dropped",
                g.flight_sent,
                g.heard[0],
                w.router.fc.visor_flight_frames_dropped()
            ));
        }
        w
    })
}
