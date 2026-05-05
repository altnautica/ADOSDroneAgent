//! MAVLink router for the lightweight agent.
//!
//! Owns the flight controller serial connection. Reads incoming MAVLink v2
//! frames, parses them via the `mavlink` crate, and broadcasts the raw bytes
//! on a `tokio::sync::broadcast` channel for in-process consumers (the cloud
//! relay client, future plugins, etc.).
//!
//! The router does not interpret message content. It is a transport — frames
//! flow in, fan out, and downstream consumers handle decoding. Outbound
//! frames from consumers (commands from the cloud relay) write back to the
//! same serial port.

#![forbid(unsafe_code)]

use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

/// Maximum size of a single MAVLink v2 frame.
const MAX_FRAME_BYTES: usize = 280;

/// Capacity of the broadcast channel that fans frames out to in-process
/// consumers. Slow consumers fall behind and lose frames — they get
/// `RecvError::Lagged` and must catch up. The cloud relay is the primary
/// consumer; FC frame rates of 30-100 Hz are well within reach.
const BROADCAST_CAPACITY: usize = 1024;

/// Capacity of the outbound queue that consumers push commands into for
/// forwarding to the FC. Small because consumers should burst infrequently.
const OUTBOUND_CAPACITY: usize = 64;

#[derive(Debug, Error)]
pub enum MavlinkError {
    #[error("serial open failed at {path}: {source}")]
    SerialOpen {
        path: String,
        #[source]
        source: tokio_serial::Error,
    },

    #[error("serial I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for the MAVLink router.
#[derive(Debug, Clone)]
pub struct MavlinkConfig {
    /// Serial port path (e.g. `/dev/ttyS0`, `/dev/ttyACM0`).
    pub port: String,
    /// Baud rate for the serial connection.
    pub baud: u32,
}

/// Handles returned by [`run_router`] so the agent can subscribe to incoming
/// frames and inject outbound commands.
pub struct RouterHandles {
    /// Subscribe to receive incoming FC frames as raw byte vectors.
    pub inbound: broadcast::Sender<Vec<u8>>,
    /// Push raw MAVLink bytes here to forward them to the FC.
    pub outbound: mpsc::Sender<Vec<u8>>,
}

/// Open the FC serial port and run the bidirectional router.
///
/// Returns immediately with handles for in-process consumers. The router
/// task runs in the background and re-opens the serial port on EOF or
/// I/O error with a 1-5 s exponential backoff, so transient FC reboots
/// or USB-CDC drops do not silently take the agent offline.
pub fn spawn_router(
    config: MavlinkConfig,
) -> Result<RouterHandles, MavlinkError> {
    // Open once up-front to validate the configuration before we declare
    // ourselves ready. The actual router task drops this stream and reopens
    // on every iteration so it has a clean restart path.
    let initial_stream = open_serial(&config)?;
    drop(initial_stream);

    let (inbound_tx, _inbound_rx) = broadcast::channel(BROADCAST_CAPACITY);
    let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);

    let inbound_for_task = inbound_tx.clone();
    let config_for_task = config.clone();
    tokio::spawn(async move {
        let mut backoff_secs: u64 = 1;
        // Single shared receiver moved into each iteration.
        let mut outbound_rx = outbound_rx;
        loop {
            let stream = match open_serial(&config_for_task) {
                Ok(s) => {
                    backoff_secs = 1;
                    s
                }
                Err(e) => {
                    tracing::warn!(error = %e, retry_in_secs = backoff_secs, "mavlink serial reopen failed");
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(5);
                    continue;
                }
            };
            tracing::info!(port = %config_for_task.port, "mavlink router connected");
            match router_loop(stream, &inbound_for_task, &mut outbound_rx).await {
                Ok(_) => {
                    tracing::warn!("mavlink router serial returned EOF; reopening");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "mavlink router I/O error; reopening");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    Ok(RouterHandles {
        inbound: inbound_tx,
        outbound: outbound_tx,
    })
}

fn open_serial(config: &MavlinkConfig) -> Result<SerialStream, MavlinkError> {
    tokio_serial::new(&config.port, config.baud)
        .timeout(Duration::from_millis(50))
        .open_native_async()
        .map_err(|source| MavlinkError::SerialOpen {
            path: config.port.clone(),
            source,
        })
}

/// The router I/O loop. Reads from serial into a frame buffer, broadcasts
/// each parsed frame, and writes inbound commands back to the FC. Returns
/// `Ok(())` on EOF (zero-byte read) so the outer reconnect loop can
/// reopen the serial port; returns `Err` on real I/O failure.
async fn router_loop(
    mut stream: SerialStream,
    inbound: &broadcast::Sender<Vec<u8>>,
    outbound: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<(), MavlinkError> {
    let mut read_buf = vec![0u8; MAX_FRAME_BYTES * 4];
    let mut frame_buf: Vec<u8> = Vec::with_capacity(MAX_FRAME_BYTES);

    loop {
        tokio::select! {
            // Serial read path: parse MAVLink frames out of the byte stream.
            read_result = stream.read(&mut read_buf) => {
                let n = read_result?;
                if n == 0 {
                    return Ok(());
                }
                frame_buf.extend_from_slice(&read_buf[..n]);
                drain_frames(&mut frame_buf, inbound);
            }
            // Outbound command path: forward to FC.
            cmd = outbound.recv() => {
                let Some(bytes) = cmd else {
                    tracing::info!("outbound channel closed; router shutting down");
                    return Ok(());
                };
                stream.write_all(&bytes).await?;
            }
        }
    }
}

/// Walk the accumulator buffer looking for MAVLink v2 frame starts (`0xFD`),
/// determine each frame's length from the header, and broadcast complete
/// frames. Drops bytes ahead of the first sync byte. Leaves partial trailing
/// frames in place for the next read.
fn drain_frames(frame_buf: &mut Vec<u8>, inbound: &broadcast::Sender<Vec<u8>>) {
    loop {
        // Discard everything before the next v2 sync byte (0xFD).
        let Some(start) = frame_buf.iter().position(|&b| b == 0xFD) else {
            frame_buf.clear();
            return;
        };
        if start > 0 {
            frame_buf.drain(..start);
        }

        // v2 header layout: STX(0xFD) LEN INC_FLAGS CMP_FLAGS SEQ SYSID COMPID MSGID(3) PAYLOAD CHECKSUM(2) SIG?(13).
        // Need at least 10 bytes for header + 2 for checksum.
        if frame_buf.len() < 12 {
            return;
        }

        let payload_len = frame_buf[1] as usize;
        let incompat_flags = frame_buf[2];
        let total_len = 10 + payload_len + 2 + if incompat_flags & 0x01 != 0 { 13 } else { 0 };
        if frame_buf.len() < total_len {
            return;
        }

        let frame: Vec<u8> = frame_buf.drain(..total_len).collect();
        // Best-effort broadcast. If no consumers are subscribed, the send
        // returns Err and we drop the frame on the floor — that's fine.
        let _ = inbound.send(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_frames_extracts_one_complete_v2_frame() {
        // Minimal valid v2 frame: STX, LEN=1, INCOMPAT=0, COMPAT=0, SEQ=0, SYS=1, COMP=1, MSGID=0,0,0, PAYLOAD=0xAB, CRC=0,0
        let frame: &[u8] = &[0xFD, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0xAB, 0x00, 0x00];
        let mut buf = frame.to_vec();
        let (tx, mut rx) = broadcast::channel(8);
        drain_frames(&mut buf, &tx);
        assert!(buf.is_empty());
        let received = rx.try_recv().expect("frame should be broadcast");
        assert_eq!(received, frame);
    }

    #[test]
    fn drain_frames_skips_garbage_before_sync_byte() {
        let frame: &[u8] = &[0xFD, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0xAB, 0x00, 0x00];
        let mut buf = vec![0xAA, 0xBB, 0xCC]; // garbage
        buf.extend_from_slice(frame);
        let (tx, mut rx) = broadcast::channel(8);
        drain_frames(&mut buf, &tx);
        assert!(buf.is_empty());
        assert_eq!(rx.try_recv().unwrap(), frame);
    }

    #[test]
    fn drain_frames_keeps_partial_trailing_frame() {
        let mut buf = vec![0xFD, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01]; // header truncated
        let (tx, mut rx) = broadcast::channel(8);
        drain_frames(&mut buf, &tx);
        assert_eq!(buf, vec![0xFD, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn drain_frames_handles_signed_frame_length() {
        // Signed v2 frame: incompat_flags=0x01 means +13 signature bytes.
        let mut frame = vec![0xFD, 0x01, 0x01, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0xAB, 0x00, 0x00];
        frame.extend_from_slice(&[0u8; 13]); // signature bytes
        let mut buf = frame.clone();
        let (tx, mut rx) = broadcast::channel(8);
        drain_frames(&mut buf, &tx);
        assert!(buf.is_empty());
        assert_eq!(rx.try_recv().unwrap(), frame);
    }
}
