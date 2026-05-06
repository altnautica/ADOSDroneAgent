//! Integration test for the inbound `mavlink/rx` topic forwarder.
//!
//! Pinned because the cloud → FC path is a thin passthrough and
//! a stray refactor that drops the forwarding (back to "log only")
//! would silently break the GCS uplink to the FC.

use ados_cloud::handlers::handle_mavlink_rx;

/// Construct a minimal MAVLink v2 HEARTBEAT frame so the test reads
/// like the operator-facing scenario: a GCS sends a heartbeat over
/// the cloud relay and it must land on the FC writer.
///
/// Frame layout (MAVLink v2):
///   byte 0:  STX = 0xFD
///   byte 1:  payload length (9 for HEARTBEAT)
///   byte 2:  incompat flags (0)
///   byte 3:  compat flags (0)
///   byte 4:  packet sequence (0)
///   byte 5:  system id (255 — GCS)
///   byte 6:  component id (190 — generic GCS-side component)
///   bytes 7-9:   message id (0 = HEARTBEAT, little-endian 24-bit)
///   bytes 10-18: payload (9 bytes)
///   bytes 19-20: CRC (placeholder; the lite passthrough never
///                validates it)
fn mavlink_v2_heartbeat() -> Vec<u8> {
    vec![
        0xFD, // STX
        0x09, // payload length
        0x00, // incompat flags
        0x00, // compat flags
        0x00, // sequence
        0xFF, // system id (GCS)
        0xBE, // component id
        0x00, 0x00, 0x00, // msg id = HEARTBEAT (0)
        // 9-byte payload
        0x00, 0x00, 0x00, 0x00, // custom_mode
        0x06, // mavtype
        0x08, // autopilot
        0x00, // base_mode
        0x03, // system_status
        0x03, // mavlink_version
        // CRC placeholder
        0xAB, 0xCD,
    ]
}

#[tokio::test]
async fn mavlink_rx_frame_lands_on_fc_writer_mpsc() {
    let (fc_tx, mut fc_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let frame = mavlink_v2_heartbeat();

    handle_mavlink_rx(frame.clone(), Some(&fc_tx));

    let received = tokio::time::timeout(std::time::Duration::from_millis(100), fc_rx.recv())
        .await
        .expect("timed out waiting for forwarded frame")
        .expect("fc writer channel closed");

    assert_eq!(
        received, frame,
        "forwarded payload must match the received bytes byte-for-byte"
    );
}

#[tokio::test]
async fn mavlink_rx_without_fc_writer_is_a_noop() {
    // No FC writer wired (offline boot; FC not yet initialised). The
    // forwarder must not panic; it logs at DEBUG and drops the frame.
    handle_mavlink_rx(mavlink_v2_heartbeat(), None);
}

#[tokio::test]
async fn mavlink_rx_drops_when_fc_writer_full() {
    // Capacity-1 channel that we deliberately fill to force the
    // try_send path. The test asserts the dispatcher does not block;
    // it logs + drops the frame.
    let (fc_tx, _fc_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    fc_tx
        .try_send(vec![0xAB; 4])
        .expect("first message fits in cap-1 channel");

    let frame = mavlink_v2_heartbeat();
    // The handler must complete synchronously without awaiting the
    // backed-up channel. The test as a whole completing is the
    // assertion.
    handle_mavlink_rx(frame, Some(&fc_tx));
}
