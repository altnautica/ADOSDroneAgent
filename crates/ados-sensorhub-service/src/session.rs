//! One connection to the hub: send `Config`, then decode and publish until
//! the stream ends or the service stops, forwarding RTCM corrections to the
//! hub in between.

use std::future::Future;

use ados_sensorbus::time::mono_ns;
use ados_sensorbus::{BusFrame, Publisher};
use ados_sensorhub::{Packet, RtcmIn, RTCM_MAX_BYTES};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::config::HubConfig;
use crate::convert::Converter;
use crate::rig::Extrinsics;
use crate::state::SharedState;
use crate::stream::{encode, StreamDecoder};
use crate::transport::HubIo;

/// Why the service asked a session to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Shutdown,
    Reload,
}

/// How a session ended.
#[derive(Debug)]
pub enum SessionEnd {
    /// The hub closed the stream.
    Closed,
    Io(std::io::Error),
    /// The hub speaks a protocol version this build cannot read.
    Unsupported(u16),
    Stopped(Stop),
}

/// What a session needs from the service.
pub struct SessionCtx<'a> {
    pub cfg: &'a HubConfig,
    pub ext: &'a Extrinsics,
    pub publisher: &'a Publisher,
    pub rtcm: &'a mut mpsc::Receiver<Vec<u8>>,
    pub state: &'a SharedState,
}

/// Run one session over `io` until it ends. Returns the end reason and the
/// number of packets delivered.
pub async fn run_session(
    io: Box<dyn HubIo>,
    ctx: SessionCtx<'_>,
    stop: impl Future<Output = Stop>,
) -> (SessionEnd, u64) {
    let (mut rd, mut wr) = tokio::io::split(io);
    let config = match encode(&Packet::Config(ctx.cfg.packet())) {
        Ok(c) => c,
        Err(e) => return (SessionEnd::Io(std::io::Error::other(e.to_string())), 0),
    };
    if let Err(e) = wr.write_all(&config).await {
        return (SessionEnd::Io(e), 0);
    }
    // Corrections queued while the link was down are stale.
    while ctx.rtcm.try_recv().is_ok() {}

    let mut conv = Converter::new(ctx.ext.clone(), ctx.cfg.gnss_forward, ctx.cfg.simulation);
    let mut dec = StreamDecoder::new();
    let crc_base = lock(ctx.state).crc_errors;
    let mut buf = vec![0u8; 4096];
    tokio::pin!(stop);
    let end = loop {
        tokio::select! {
            biased;
            why = &mut stop => break SessionEnd::Stopped(why),
            read = rd.read(&mut buf) => {
                let n = match read {
                    Ok(0) => break SessionEnd::Closed,
                    Ok(n) => n,
                    Err(e) => break SessionEnd::Io(e),
                };
                let arrival = mono_ns();
                {
                    let mut st = lock(ctx.state);
                    let mut emit = |f: BusFrame| {
                        ctx.publisher.send(&f);
                    };
                    dec.push(&buf[..n], |p| conv.handle(&p, arrival, &mut st, &mut emit));
                    st.crc_errors = crc_base + dec.stats().corrupt();
                }
                if let Some(v) = conv.unsupported_proto {
                    break SessionEnd::Unsupported(v);
                }
            }
            Some(bytes) = ctx.rtcm.recv() => {
                if let Err(e) = forward_rtcm(&mut wr, conv.rtcm_port(), &bytes).await {
                    break SessionEnd::Io(e);
                }
            }
        }
    };
    let stats = dec.stats();
    if stats.bad_payloads > 0 || stats.unknown > 0 {
        tracing::info!(
            bad_payloads = stats.bad_payloads,
            unknown = stats.unknown,
            "hub packets skipped this session"
        );
    }
    (end, stats.packets)
}

/// Write `bytes` to the hub as `RtcmIn` packets of at most the packet's
/// payload limit.
async fn forward_rtcm<W: AsyncWrite + Unpin>(
    wr: &mut W,
    port: u8,
    bytes: &[u8],
) -> std::io::Result<()> {
    for chunk in bytes.chunks(RTCM_MAX_BYTES) {
        let frame = encode(&Packet::RtcmIn(RtcmIn { port, bytes: chunk }))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        wr.write_all(&frame).await?;
    }
    Ok(())
}

/// Lock the shared state; a poisoned lock still holds valid counters.
pub fn lock(state: &SharedState) -> std::sync::MutexGuard<'_, crate::state::HubState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_sensorbus::Origin;
    use ados_sensorhub::{Hello, ImuBatch, ImuSample, ImuSamples};

    #[tokio::test]
    async fn an_unsupported_hello_ends_the_session_without_reading_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let publisher = Publisher::spawn(dir.path().join("sensors.sock"), Origin::Hub);
        let (_tx, mut rtcm) = mpsc::channel(1);
        let state = SharedState::default();
        let cfg = HubConfig::default();
        let ext = Extrinsics::default();
        let (host, mut hub) = tokio::io::duplex(1 << 16);

        // One write: a v2 Hello, then an IMU batch the v1 decoder could parse.
        let samples = [ImuSample::default(); 4];
        let mut wire = encode(&Packet::Hello(Hello {
            proto_ver: 2,
            fw_major: 1,
            fw_minor: 0,
            fw_patch: 0,
            board_id: 0,
            caps: 0,
            uid: [0; 12],
            board_name: [0; 16],
        }))
        .unwrap();
        wire.extend(
            encode(&Packet::ImuBatch(ImuBatch {
                t0_ns: 1,
                dt_ns: 1_000_000,
                imu_id: 0,
                flags: 0,
                seq0: 0,
                samples: ImuSamples::Typed(&samples),
            }))
            .unwrap(),
        );
        hub.write_all(&wire).await.unwrap();

        let ctx = SessionCtx {
            cfg: &cfg,
            ext: &ext,
            publisher: &publisher,
            rtcm: &mut rtcm,
            state: &state,
        };
        let (end, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_session(Box::new(host), ctx, std::future::pending()),
        )
        .await
        .unwrap();
        assert!(matches!(end, SessionEnd::Unsupported(2)), "{end:?}");
        let st = lock(&state);
        assert_eq!(st.imu_samples, 0);
        assert_eq!(st.hello.as_ref().map(|h| h.proto_ver), Some(2));
    }
}
