//! RTCM corrections from the sensor bus into the flight controller.
//!
//! Every `rtcm` frame on the bus (a GNSS base, the REST upload, a hub
//! receiver) is split into whole RTCM3 messages and each message into
//! `GPS_RTCM_DATA` fragments by one persistent [`Fragmenter`], so the 5-bit
//! sequence id advances across the router's whole lifetime as the firmware's
//! reassembly expects.

use std::sync::Arc;
use std::time::Duration;

use ados_protocol::mavlink::ardupilotmega::GPS_RTCM_DATA_DATA;
use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::rtcm::{split_rtcm3, Fragmenter};
use ados_sensorbus::{Kind, Subscriber};

use crate::connection::FcConnection;

/// Pause before re-subscribing after the bus went away or refused us.
const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// The `GPS_RTCM_DATA` messages for one bus payload, in send order. A message
/// too long for four fragments is dropped (and logged), the rest still go.
pub fn rtcm_messages(fragmenter: &mut Fragmenter, payload: &[u8]) -> Vec<GPS_RTCM_DATA_DATA> {
    let mut out = Vec::new();
    for msg in split_rtcm3(payload) {
        match fragmenter.fragment(msg) {
            Ok(frags) => out.extend(frags.into_iter().map(|f| GPS_RTCM_DATA_DATA {
                flags: f.flags,
                len: f.len,
                data: f.data,
            })),
            Err(e) => tracing::warn!(error = %e, "rtcm_message_dropped_too_long"),
        }
    }
    out
}

/// Subscribe to `rtcm` on the bus at `path` and inject every frame into the FC
/// until `cancel`, re-subscribing whenever the bus is lost.
pub async fn run(fc: Arc<FcConnection>, path: std::path::PathBuf, cancel: Shutdown) {
    let mut fragmenter = Fragmenter::default();
    loop {
        let sub = tokio::select! {
            s = Subscriber::connect(&path, &[Kind::Rtcm]) => s,
            _ = cancel.wait() => return,
        };
        match sub {
            Ok(mut sub) => loop {
                let frame = tokio::select! {
                    f = sub.recv() => f,
                    _ = cancel.wait() => return,
                };
                match frame {
                    Ok(frame) if frame.kind == Kind::Rtcm => {
                        for msg in rtcm_messages(&mut fragmenter, &frame.payload) {
                            fc.send_rtcm_fragment(msg).await;
                        }
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "rtcm_subscription_lost");
                        break;
                    }
                }
            },
            Err(e) => tracing::debug!(error = %e, "rtcm_subscribe_failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(RESUBSCRIBE) => {}
            _ = cancel.wait() => return,
        }
    }
}
