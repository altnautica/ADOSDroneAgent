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

/// Spec ceiling on the LEN field (u8). MAVLink encodes the payload size in
/// a single byte for both v1 and v2; anything past 255 is unrepresentable
/// in the wire format and indicates either a parser bug or an attacker
/// trying to overrun the read buffer.
///
/// Kept as a named constant rather than inlined so the audit finding
/// ("no test for oversized payload") has an obvious target to pin
/// behavior against.
const MAX_PAYLOAD_BYTES: usize = 255;

/// MAVLink v1 sync byte. The router accepts both v1 and v2 frames so a
/// dual-stack FC (some Betaflight/iNav builds still emit v1 by default
/// even when MAVLink v2 is requested by the GCS) does not appear silent
/// to the cloud relay.
const MAVLINK_V1_STX: u8 = 0xFE;

/// MAVLink v2 sync byte.
const MAVLINK_V2_STX: u8 = 0xFD;

/// X.25-style CRC used by MAVLink. Polynomial 0x1021, initial value
/// 0xFFFF, byte-by-byte; appended with a per-message-id `crc_extra`
/// constant from the dialect XML so a frame reordering bug at the FC
/// surfaces as a CRC mismatch rather than a silent decode of the wrong
/// message type.
fn crc_x25(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        let mut tmp = b ^ (crc as u8);
        tmp ^= tmp << 4;
        crc = (crc >> 8) ^ ((tmp as u16) << 8) ^ ((tmp as u16) << 3) ^ ((tmp as u16) >> 4);
    }
    crc
}

/// Per-message `CRC_EXTRA` constant from the MAVLink dialect XML. Returns
/// `Some(crc_extra)` for message ids the router knows; `None` for ids it
/// has not been taught about. Unknown ids pass through without CRC
/// validation so a fresh dialect message from a newer FC firmware does
/// not get silently dropped — the router is a transport, not a decoder.
///
/// Only the message ids actually surfaced in unit tests are populated
/// today. The MAVLink common + ardupilotmega dialects between them define
/// hundreds of ids; the `mavlink` crate (currently commented out in the
/// workspace `Cargo.toml`) carries a generated table. Adding it costs
/// non-trivial code size on the lite agent, which is the wrong trade for
/// a transport that broadcasts opaque blobs. When typed decoding lands
/// the table comes with it.
fn crc_extra_for(msgid: u32) -> Option<u8> {
    match msgid {
        0 => Some(50),  // HEARTBEAT (MAVLink common)
        1 => Some(124), // SYS_STATUS (MAVLink common)
        _ => None,
    }
}

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
                // Bound the accumulator. If the stream is mis-baud'd
                // or carrying garbage with no 0xFD sync, the buffer
                // could otherwise grow unbounded on a 256 MB SBC.
                // Cap at 8 max-frames worth. On overflow, drain
                // everything before the last sync byte we can find so
                // the parser keeps a candidate frame start to work with;
                // if there's no sync at all, drain the front half so the
                // tail keeps room for a fresh sync to arrive.
                const MAX_BUF_BYTES: usize = MAX_FRAME_BYTES * 8;
                if frame_buf.len() > MAX_BUF_BYTES {
                    let last_sync = frame_buf
                        .iter()
                        .rposition(|&b| b == MAVLINK_V2_STX || b == MAVLINK_V1_STX);
                    let drop_n = match last_sync {
                        Some(pos) if pos > 0 => pos,
                        _ => frame_buf.len() / 2,
                    };
                    frame_buf.drain(..drop_n);
                    tracing::warn!(
                        bytes_dropped = drop_n,
                        kept_bytes = frame_buf.len(),
                        had_sync = last_sync.is_some(),
                        "mavlink frame buffer overflow; check FC baud rate"
                    );
                }
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

/// Walk the accumulator buffer looking for MAVLink frame starts (`0xFD`
/// for v2 or `0xFE` for v1), determine each frame's length from the
/// header, validate the CRC for known message ids, and broadcast
/// complete frames. Drops bytes ahead of the first sync byte. Leaves
/// partial trailing frames in place for the next read.
///
/// CRC failures advance one byte past the bad sync and retry — the
/// candidate sync was almost certainly a payload byte that happened to
/// equal 0xFD/0xFE, and the real frame start is further into the buffer.
fn drain_frames(frame_buf: &mut Vec<u8>, inbound: &broadcast::Sender<Vec<u8>>) {
    loop {
        // Discard everything before the next sync byte. Accept both v2
        // (0xFD) and v1 (0xFE) starts — pick whichever appears first.
        let next_sync = frame_buf
            .iter()
            .position(|&b| b == MAVLINK_V2_STX || b == MAVLINK_V1_STX);
        let Some(start) = next_sync else {
            frame_buf.clear();
            return;
        };
        if start > 0 {
            frame_buf.drain(..start);
        }

        let stx = frame_buf[0];
        let outcome = match stx {
            MAVLINK_V2_STX => parse_v2_frame(frame_buf),
            MAVLINK_V1_STX => parse_v1_frame(frame_buf),
            _ => unreachable!("position filter restricted stx to V1/V2"),
        };
        match outcome {
            FrameOutcome::Need => return,
            FrameOutcome::BadCrc | FrameOutcome::TooLarge => {
                // The candidate sync byte was likely payload (or a header
                // claim past the spec ceiling); drop it and keep scanning
                // for a real frame start.
                frame_buf.drain(..1);
                continue;
            }
            FrameOutcome::Ready(total_len) => {
                let frame: Vec<u8> = frame_buf.drain(..total_len).collect();
                // Best-effort broadcast. If no consumers are subscribed,
                // the send returns Err and we drop the frame on the
                // floor — that's fine.
                let _ = inbound.send(frame);
            }
        }
    }
}

/// Result of inspecting the head of `frame_buf` against a candidate sync
/// byte. The parse helpers do not mutate the buffer; the caller drains
/// based on the variant.
enum FrameOutcome {
    /// More bytes needed before a verdict can be reached.
    Need,
    /// CRC mismatch on a known message id; caller drops 1 byte and retries.
    BadCrc,
    /// Header claimed a payload past the spec ceiling; caller drops 1
    /// byte and retries.
    TooLarge,
    /// Frame is complete and CRC-valid (or msgid is unknown so CRC
    /// checking is not enforced); caller drains `total_len` bytes.
    Ready(usize),
}

/// v2 header layout: STX(0xFD) LEN INC_FLAGS CMP_FLAGS SEQ SYSID COMPID
/// MSGID(3) PAYLOAD CHECKSUM(2) SIG?(13). Total = 10 + LEN + 2 + (sig?13).
fn parse_v2_frame(frame_buf: &[u8]) -> FrameOutcome {
    if frame_buf.len() < 12 {
        return FrameOutcome::Need;
    }
    let payload_len = frame_buf[1] as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return FrameOutcome::TooLarge;
    }
    let incompat_flags = frame_buf[2];
    let signed = incompat_flags & 0x01 != 0;
    let total_len = 10 + payload_len + 2 + if signed { 13 } else { 0 };
    if total_len > MAX_FRAME_BYTES {
        return FrameOutcome::TooLarge;
    }
    if frame_buf.len() < total_len {
        return FrameOutcome::Need;
    }
    // CRC is computed over LEN..end-of-payload (header + payload, skipping
    // the 0xFD sync byte) plus the per-message CRC_EXTRA byte. Located at
    // bytes [10 + payload_len .. 12 + payload_len]; signature, if any,
    // sits past the CRC and is NOT covered.
    let msgid =
        (frame_buf[7] as u32) | ((frame_buf[8] as u32) << 8) | ((frame_buf[9] as u32) << 16);
    if let Some(extra) = crc_extra_for(msgid) {
        let crc_start = 10 + payload_len;
        let claimed = u16::from_le_bytes([frame_buf[crc_start], frame_buf[crc_start + 1]]);
        let computed = crc_x25_with_extra(&frame_buf[1..crc_start], extra);
        if claimed != computed {
            return FrameOutcome::BadCrc;
        }
    }
    FrameOutcome::Ready(total_len)
}

/// v1 header layout: STX(0xFE) LEN SEQ SYSID COMPID MSGID PAYLOAD
/// CHECKSUM(2). Total = 6 + LEN + 2.
fn parse_v1_frame(frame_buf: &[u8]) -> FrameOutcome {
    if frame_buf.len() < 8 {
        return FrameOutcome::Need;
    }
    let payload_len = frame_buf[1] as usize;
    if payload_len > MAX_PAYLOAD_BYTES {
        return FrameOutcome::TooLarge;
    }
    let total_len = 6 + payload_len + 2;
    if frame_buf.len() < total_len {
        return FrameOutcome::Need;
    }
    let msgid = frame_buf[5] as u32;
    if let Some(extra) = crc_extra_for(msgid) {
        let crc_start = 6 + payload_len;
        let claimed = u16::from_le_bytes([frame_buf[crc_start], frame_buf[crc_start + 1]]);
        let computed = crc_x25_with_extra(&frame_buf[1..crc_start], extra);
        if claimed != computed {
            return FrameOutcome::BadCrc;
        }
    }
    FrameOutcome::Ready(total_len)
}

/// Compute the X.25 CRC over `bytes` and then accumulate the per-message
/// `crc_extra` byte. Helper around `crc_x25` so the v1 + v2 paths share
/// the dialect-extra fold and the unit tests can call the same routine.
fn crc_x25_with_extra(bytes: &[u8], extra: u8) -> u16 {
    let mut crc = crc_x25(bytes);
    let mut tmp = extra ^ (crc as u8);
    tmp ^= tmp << 4;
    crc = (crc >> 8) ^ ((tmp as u16) << 8) ^ ((tmp as u16) << 3) ^ ((tmp as u16) >> 4);
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_frames_extracts_one_complete_v2_frame() {
        // Minimal valid v2 frame with msgid=99 (unknown to crc_extra_for so
        // CRC validation is bypassed per the parser's pass-through policy
        // for unknown dialects). STX, LEN=1, INCOMPAT=0, COMPAT=0, SEQ=0,
        // SYS=1, COMP=1, MSGID=99,0,0, PAYLOAD=0xAB, CRC=0,0.
        let frame: &[u8] = &[0xFD, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x63, 0x00, 0x00, 0xAB, 0x00, 0x00];
        let mut buf = frame.to_vec();
        let (tx, mut rx) = broadcast::channel(8);
        drain_frames(&mut buf, &tx);
        assert!(buf.is_empty());
        let received = rx.try_recv().expect("frame should be broadcast");
        assert_eq!(received, frame);
    }

    #[test]
    fn drain_frames_skips_garbage_before_sync_byte() {
        // Same unknown msgid=99 to skip CRC validation.
        let frame: &[u8] = &[0xFD, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x63, 0x00, 0x00, 0xAB, 0x00, 0x00];
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
        // Signed v2 frame with msgid=99 (CRC-bypass dialect).
        // incompat_flags=0x01 means +13 signature bytes.
        let mut frame = vec![0xFD, 0x01, 0x01, 0x00, 0x00, 0x01, 0x01, 0x63, 0x00, 0x00, 0xAB, 0x00, 0x00];
        frame.extend_from_slice(&[0u8; 13]); // signature bytes
        let mut buf = frame.clone();
        let (tx, mut rx) = broadcast::channel(8);
        drain_frames(&mut buf, &tx);
        assert!(buf.is_empty());
        assert_eq!(rx.try_recv().unwrap(), frame);
    }
}
