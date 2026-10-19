//! RTCM corrections from the sensor bus into the flight controller.
//!
//! Every `rtcm` frame on the bus (a GNSS base, the REST upload, a hub
//! receiver) is a piece of its source's RTCM3 byte stream: one
//! [`Rtcm3Splitter`] per bus `src` reassembles whole CRC-valid messages, so a
//! message split across two bus frames is injected once, whole. Each message
//! becomes `GPS_RTCM_DATA` fragments from one persistent [`Fragmenter`], so the
//! 5-bit sequence id advances across the router's whole lifetime as the
//! firmware's reassembly expects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::mavlink::ardupilotmega::GPS_RTCM_DATA_DATA;
use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::rtcm::{Fragmenter, Rtcm3Splitter};
use ados_sensorbus::{Kind, Subscriber};

use crate::connection::FcConnection;

/// Pause before re-subscribing after the bus went away or refused us.
const RESUBSCRIBE: Duration = Duration::from_secs(1);

/// Sources tracked at once; past this the partial frames are dropped and the
/// map starts over (a few RTCM sources exist on any node).
const MAX_SOURCES: usize = 16;

/// The router's RTCM state: a stream splitter per bus source and the shared
/// fragment sequence.
#[derive(Debug, Default)]
pub struct RtcmInjector {
    fragmenter: Fragmenter,
    splitters: HashMap<String, Rtcm3Splitter>,
}

impl RtcmInjector {
    /// The `GPS_RTCM_DATA` messages the bytes `payload` from source `src`
    /// complete, in send order. A message too long for four fragments is
    /// dropped (and logged), the rest still go.
    pub fn messages(&mut self, src: &str, payload: &[u8]) -> Vec<GPS_RTCM_DATA_DATA> {
        if !self.splitters.contains_key(src) && self.splitters.len() >= MAX_SOURCES {
            self.splitters.clear();
        }
        let frames = self
            .splitters
            .entry(src.to_string())
            .or_default()
            .push(payload);
        let mut out = Vec::new();
        for msg in frames {
            match self.fragmenter.fragment(&msg) {
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

    /// Drop every partial frame (the stream had a gap).
    pub fn reset_streams(&mut self) {
        self.splitters.clear();
    }
}

/// Subscribe to `rtcm` on the bus at `path` and inject every frame into the FC
/// until `cancel`, re-subscribing whenever the bus is lost.
pub async fn run(fc: Arc<FcConnection>, path: std::path::PathBuf, cancel: Shutdown) {
    let mut injector = RtcmInjector::default();
    loop {
        injector.reset_streams();
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
                        for msg in injector.messages(&frame.src, &frame.payload) {
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
