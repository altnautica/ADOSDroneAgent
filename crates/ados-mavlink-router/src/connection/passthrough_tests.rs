//! Transparent MSP passthrough: for an MSP FC (Betaflight/iNav) the raw
//! FC->host bytes travel on the raw lane so a polling MSP GCS receives the
//! FC's responses, while a MAVLink FC's frame path stays byte-unchanged and
//! never touches the raw lane.
use super::*;
use crate::param_cache::ParamCache;
use crate::state::VehicleState;
use ados_protocol::mavlink::ardupilotmega::{
    MavAutopilot, MavModeFlag, MavState, MavType, HEARTBEAT_DATA,
};
use tokio::sync::broadcast::error::TryRecvError;

fn conn() -> std::sync::Arc<FcConnection> {
    let state = std::sync::Arc::new(Mutex::new(VehicleState::default()));
    let params = std::sync::Arc::new(Mutex::new(ParamCache::new(
        "/tmp/ados-passthrough-params.json",
    )));
    FcConnection::new(MavlinkConfig::default(), state, params)
}

/// A real MAVLink v2 HEARTBEAT frame (same constructor as the framing tests).
fn heartbeat_frame() -> Vec<u8> {
    let msg = MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_QUADROTOR,
        autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    });
    mavlink::serialize_v2(
        MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 0,
        },
        &msg,
    )
    .unwrap()
}

/// A reader that yields its pre-set chunks one poll at a time, then EOF. Lets
/// a test drive `read_loop` with a frame split across two reads to exercise
/// the reassembly path.
struct ChunkReader {
    chunks: std::collections::VecDeque<Vec<u8>>,
}
impl ChunkReader {
    fn new(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks: chunks.into_iter().collect(),
        }
    }
}
impl tokio::io::AsyncRead for ChunkReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Some(chunk) = this.chunks.pop_front() {
            let n = chunk.len().min(buf.remaining());
            buf.put_slice(&chunk[..n]);
            if n < chunk.len() {
                this.chunks.push_front(chunk[n..].to_vec());
            }
        }
        // No chunk left (or a zero-remaining buffer): 0 bytes filled reads as
        // EOF, so `read_loop` returns rather than hanging.
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn each_decoded_fc_frame_advances_the_work_counter_and_proves_the_link() {
    // The supervisor judges this service on the counter, not on process
    // I/O (which the 10 Hz state publish keeps moving even with a wedged
    // reader), so every frame off the link must count, and only those.
    let c = conn();
    assert_eq!(c.frames_decoded(), 0);
    let mut bytes = heartbeat_frame();
    bytes.extend(heartbeat_frame());
    bytes.extend([0x00, 0x55, 0xAA]); // line noise decodes nothing
    c.read_loop(Box::pin(std::io::Cursor::new(bytes))).await;
    assert_eq!(c.frames_decoded(), 2);
    assert!(c.link_proven.load(Ordering::Relaxed));
}

#[tokio::test]
async fn msp_fc_raw_bytes_reach_the_raw_lane_and_not_the_frame_lane() {
    let c = conn();
    // As fc_variant_for_port would set it at open for a Betaflight board.
    *c.fc_variant.lock().await = Some("betaflight".into());
    let mut raw_rx = c.subscribe_raw();
    let mut frame_rx = c.subscribe();
    // A representative MSP `$M>` response with no MAVLink magic (0xFD/0xFE).
    let msp = b"\x24\x4d\x3e\x02\x64\x01\x02\x67".to_vec();
    c.read_loop(Box::pin(std::io::Cursor::new(msp.clone())))
        .await;
    // The exact raw bytes are forwarded on the raw lane...
    assert_eq!(raw_rx.try_recv().unwrap(), msp);
    // ...and the frame lane stays silent (MSP yields no MAVLink frame).
    assert!(matches!(frame_rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn mavlink_fc_frame_path_is_unchanged_and_raw_lane_is_silent() {
    let c = conn();
    // A MAVLink FC has no USB-descriptor variant → is_msp is false.
    assert!(c.fc_variant.lock().await.is_none());
    let mut raw_rx = c.subscribe_raw();
    let mut frame_rx = c.subscribe();
    let frame = heartbeat_frame();
    c.read_loop(Box::pin(std::io::Cursor::new(frame.clone())))
        .await;
    // The exact frame bytes, verbatim, no re-encode.
    assert_eq!(frame_rx.try_recv().unwrap(), frame);
    // The raw lane never fires for a MAVLink FC.
    assert!(matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn a_mavlink_frame_split_across_two_reads_lands_once_on_the_frame_lane() {
    let c = conn();
    let mut raw_rx = c.subscribe_raw();
    let mut frame_rx = c.subscribe();
    let frame = heartbeat_frame();
    let split = frame.len() / 2;
    let reader = ChunkReader::new(vec![frame[..split].to_vec(), frame[split..].to_vec()]);
    c.read_loop(Box::pin(reader)).await;
    // Reassembled across the two reads into exactly one frame.
    assert_eq!(frame_rx.try_recv().unwrap(), frame);
    assert!(
        matches!(frame_rx.try_recv(), Err(TryRecvError::Empty)),
        "exactly one frame, not duplicated"
    );
    assert!(matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn an_msp_chunk_with_an_embedded_frame_start_is_forwarded_verbatim_once() {
    let c = conn();
    *c.fc_variant.lock().await = Some("inav".into());
    let mut raw_rx = c.subscribe_raw();
    // A chunk that embeds a stray `$M>` mid-stream is still forwarded whole.
    let chunk = b"\x01\x02\x24\x4d\x3e\x05\x06".to_vec();
    c.read_loop(Box::pin(std::io::Cursor::new(chunk.clone())))
        .await;
    assert_eq!(raw_rx.try_recv().unwrap(), chunk);
    assert!(
        matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)),
        "exactly one raw chunk"
    );
}

#[tokio::test]
async fn an_msp_chunk_containing_a_mavlink_magic_byte_never_carves_a_garbage_frame() {
    let c = conn();
    *c.fc_variant.lock().await = Some("betaflight".into());
    let mut raw_rx = c.subscribe_raw();
    let mut frame_rx = c.subscribe();
    // An MSP response whose payload contains the MAVLink v2 magic 0xFD
    // followed by a plausible length byte. extract_frames does NO CRC check,
    // so without the MSP fast-path it would carve a garbage "frame" from
    // these bytes and fan it onto the frame lane alongside the raw copy,
    // corrupting the MSP stream a GCS sees. The frame lane MUST stay silent.
    let mut msp = b"\x24\x4d\x3e\x08\xfd\x00\x00".to_vec();
    msp.extend(std::iter::repeat_n(0xAA, 20));
    c.read_loop(Box::pin(std::io::Cursor::new(msp.clone())))
        .await;
    // The full MSP chunk is forwarded once, verbatim, on the raw lane.
    assert_eq!(raw_rx.try_recv().unwrap(), msp);
    assert!(matches!(raw_rx.try_recv(), Err(TryRecvError::Empty)));
    // No garbage frame is ever carved onto the frame lane for an MSP FC.
    assert!(
        matches!(frame_rx.try_recv(), Err(TryRecvError::Empty)),
        "an MSP FC must never emit a MAVLink frame, even when its payload contains 0xFD/0xFE"
    );
}

/// End-to-end over the two byte-lane IPC sockets the router serves: an MSP FC's
/// FC->host bytes reach a client on the MSP socket (via the raw lane producer),
/// while a client on the MAVLink socket stays silent (the frame lane never
/// fires for an MSP FC). Stands up both sockets, wires the two producers exactly
/// as the router's `main()`, drives a real MSP `read_loop`, and observes both.
#[tokio::test]
async fn the_msp_socket_carries_raw_bytes_while_the_mavlink_socket_stays_silent() {
    use ados_protocol::frame::{encode_frame, MAVLINK_MAX_FRAME};
    use ados_protocol::ipc::{connect_with_retry, read_length_prefixed, IpcBroadcast};

    let dir = std::env::temp_dir();
    let msp_path = dir.join(format!("ados-mspplane-{}.sock", std::process::id()));
    let mav_path = dir.join(format!("ados-mavplane-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&msp_path);
    let _ = std::fs::remove_file(&mav_path);

    // Both sockets as the router binds them: length-prefixed, 256-deep, with an
    // inbound channel (unused here — this test exercises only the FC->client path).
    let (msp_ipc, _msp_inbound) = IpcBroadcast::bind(&msp_path, 256, false, Some(256))
        .await
        .unwrap();
    let msp_ipc = std::sync::Arc::new(msp_ipc);
    let (mav_ipc, _mav_inbound) = IpcBroadcast::bind(&mav_path, 256, false, Some(256))
        .await
        .unwrap();
    let mav_ipc = std::sync::Arc::new(mav_ipc);

    // An MSP FC, so the read loop routes FC->host bytes onto the raw lane.
    let c = conn();
    *c.fc_variant.lock().await = Some("betaflight".into());

    let cancel = Shutdown::new();

    // Raw lane -> MSP socket (the new producer, verbatim from `main()`).
    {
        let msp_ipc = msp_ipc.clone();
        let cancel = cancel.clone();
        let mut raw_rx = c.subscribe_raw();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    chunk = raw_rx.recv() => match chunk {
                        Ok(bytes) => {
                            if let Ok(framed) = encode_frame(&bytes, MAVLINK_MAX_FRAME) {
                                msp_ipc.broadcast(framed.into()).await;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = cancel.wait() => break,
                }
            }
        });
    }
    // Frame lane -> MAVLink socket (the existing producer, verbatim from `main()`).
    {
        let mav_ipc = mav_ipc.clone();
        let cancel = cancel.clone();
        let mut frame_rx = c.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = frame_rx.recv() => match frame {
                        Ok(f) => {
                            if let Ok(framed) = encode_frame(&f, MAVLINK_MAX_FRAME) {
                                mav_ipc.broadcast(framed.into()).await;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = cancel.wait() => break,
                }
            }
        });
    }

    // A client on each socket, registered before any broadcast fires.
    let mut msp_client = connect_with_retry(&msp_path, 20, Duration::from_millis(20))
        .await
        .unwrap();
    let mut mav_client = connect_with_retry(&mav_path, 20, Duration::from_millis(20))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Drive the MSP FC: read_loop forwards the raw chunk onto the raw lane.
    let msp = b"\x24\x4d\x3e\x02\x64\x01\x02\x67".to_vec();
    c.read_loop(Box::pin(std::io::Cursor::new(msp.clone())))
        .await;

    // The MSP socket client receives the exact MSP bytes (length-prefixed on the
    // wire, de-framed back to the original chunk).
    let got = tokio::time::timeout(
        Duration::from_secs(1),
        read_length_prefixed(&mut msp_client, MAVLINK_MAX_FRAME, false),
    )
    .await
    .expect("the msp client read must not time out")
    .unwrap();
    assert_eq!(got.as_deref(), Some(&msp[..]));

    // The MAVLink socket client stays silent: an MSP FC never puts a frame on
    // the frame lane, so nothing is ever broadcast to it.
    let silent = tokio::time::timeout(
        Duration::from_millis(200),
        read_length_prefixed(&mut mav_client, MAVLINK_MAX_FRAME, false),
    )
    .await;
    assert!(
        silent.is_err(),
        "the mavlink socket must stay silent for an MSP FC"
    );

    cancel.trigger();
    let _ = std::fs::remove_file(&msp_path);
    let _ = std::fs::remove_file(&mav_path);
}
