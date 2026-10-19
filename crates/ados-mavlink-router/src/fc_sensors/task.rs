//! The sensor lane task: reads the FC fan-out, keeps the TIMESYNC exchange
//! and the capture marker going, and publishes onto the sensor bus.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::mavlink::{self, ardupilotmega::MavMessage};
use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::{Kind, Publisher};
use tokio::sync::broadcast::error::RecvError;

use super::lane::{BusOut, LaneCore, BARO_SRC, TRIGGER_SRC};
use crate::connection::FcConnection;
use crate::rx_frame::RxFrame;

/// Bus stream id of the FC TIMESYNC pairs.
pub const SYNC_SRC: &str = "fc";

/// Message ids the lane decodes; every other frame is skipped on its header.
const SENSOR_MSG_IDS: &[u32] = &[
    105, // HIGHRES_IMU
    27,  // RAW_IMU
    29,  // SCALED_PRESSURE
    2,   // SYSTEM_TIME
    180, // CAMERA_FEEDBACK
    111, // TIMESYNC
];

/// Cadence of the TIMESYNC request and the capture-marker poll.
const SLOW_TICK: Duration = Duration::from_secs(1);

/// Whether this profile exchanges TIMESYNC with its FC.
pub fn sends_timesync(profile: &str) -> bool {
    matches!(profile, "drone" | "visor")
}

/// Run the lane until `cancel`.
pub async fn run(
    fc: Arc<FcConnection>,
    publisher: Publisher,
    capture_marker: PathBuf,
    cancel: Shutdown,
) {
    let mut frames = fc.subscribe();
    let mut core = LaneCore::default();
    let mut out = Vec::new();
    let timesync = sends_timesync(fc.profile());
    let mut flush = tokio::time::interval(Duration::from_millis(10));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut slow = tokio::time::interval(SLOW_TICK);
    slow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            r = frames.recv() => match r {
                Ok(frame) => on_frame(&fc, &mut core, &frame, &mut out),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            _ = flush.tick() => core.flush_due(ados_sensorbus::time::mono_ns(), &mut out),
            _ = slow.tick() => {
                fc.sensor_streams().set_capture_active(capture_marker.exists());
                if timesync {
                    let ts1 = ados_sensorbus::time::mono_ns();
                    if fc.send_timesync(ts1).await {
                        core.timesync_sent(ts1);
                    }
                }
            }
            _ = cancel.wait() => return,
        }
        for item in out.drain(..) {
            publish(&publisher, item);
        }
    }
}

/// Feed one fan-out frame to the lane when it is a sensor message from the
/// vehicle this router is attached to.
fn on_frame(fc: &FcConnection, core: &mut LaneCore, frame: &RxFrame, out: &mut Vec<BusOut>) {
    let Some(id) = crate::aux_tee::mavlink_message_id(frame) else {
        return;
    };
    if !SENSOR_MSG_IDS.contains(&id) {
        return;
    }
    // Only a local, identified FC: a relayed vehicle's frames on a ground
    // station are not this host's sensors.
    let Some((sys, comp)) = fc.learned_fc_identity() else {
        return;
    };
    let Ok((header, msg)) = mavlink::parse_any(frame) else {
        return;
    };
    if header.system_id != sys || header.component_id != comp {
        return;
    }
    if matches!(msg, MavMessage::HIGHRES_IMU(_)) {
        fc.sensor_streams().highres_seen();
    }
    core.on_message(frame.rx_ns, &msg, out);
}

fn publish(publisher: &Publisher, item: BusOut) {
    match item {
        BusOut::Imu { src, batch } => publisher.proto(Kind::Imu, src, &batch),
        BusOut::Baro(b) => publisher.msgpack(Kind::Baro, BARO_SRC, &b),
        BusOut::Trigger(t) => publisher.proto(Kind::Trigger, TRIGGER_SRC, &t),
        BusOut::Sync(s) => publisher.msgpack(Kind::Sync, SYNC_SRC, &s),
    };
}
