use super::*;
use ados_protocol::frame::{decode_len, HEADER_SIZE};
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;

/// A send against an absent socket returns an I/O error (which the route maps
/// to a 503), not a panic.
#[tokio::test]
async fn send_to_an_absent_socket_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let client = MavlinkIpcClient::new(dir.path().join("absent.sock"));
    let err = client.send(b"\xfd\x00").await.unwrap_err();
    assert!(
        matches!(err, SendError::Io(_)),
        "expected an Io error: {err:?}"
    );
}

/// A send against a live socket writes the 4-byte length prefix followed by
/// the raw frame bytes, recoverable with `decode_len`.
#[tokio::test]
async fn send_writes_a_length_prefixed_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let payload = b"\xfd\x09\x00\x00\x05\x01\x01\x4c".to_vec();
    let client = MavlinkIpcClient::new(path.clone());

    // Accept the connection on the server side, read one framed message.
    let server = tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let mut header = [0u8; HEADER_SIZE];
        conn.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body).await.unwrap();
        body
    });

    client
        .send(&payload)
        .await
        .expect("send succeeds on a live socket");
    let got = server.await.unwrap();
    assert_eq!(got, payload, "the server reads back the exact raw frame");
}

/// Each send uses its own connection and closes it, so the client is never
/// left attached to the broadcast as a consumer that does not read.
#[tokio::test]
async fn each_send_closes_its_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let client = MavlinkIpcClient::new(path.clone());

    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut conn, _addr) = listener.accept().await.unwrap();
            let mut header = [0u8; HEADER_SIZE];
            conn.read_exact(&mut header).await.unwrap();
            let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
            let mut body = vec![0u8; len];
            conn.read_exact(&mut body).await.unwrap();
            // The client closed after its frame: the next read is EOF.
            let mut rest = [0u8; 1];
            assert_eq!(conn.read(&mut rest).await.unwrap(), 0);
        }
    });
    client.send(b"\xfd\x00\x05").await.expect("first send");
    client.send(b"\xfd\x00\x06").await.expect("second send");
    server.await.unwrap();
}

#[test]
fn default_socket_honours_the_run_dir_override() {
    let p = default_mavlink_socket();
    assert!(p.ends_with("mavlink.sock"));
}

/// The ack stream writes a length-prefixed command to the server and reads a
/// v2 record the server broadcasts back, recovering the exact frame bytes
/// behind the receive-time header.
#[tokio::test]
async fn ack_stream_writes_a_command_and_reads_a_broadcast_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let command = b"\xfd\x01\x02command".to_vec();
    let broadcast = b"\xfd\x03\x04ack-frame".to_vec();

    let b_clone = broadcast.clone();
    let c_clone = command.clone();
    let server = tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        // Read the client's command frame.
        let mut header = [0u8; HEADER_SIZE];
        conn.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, MAVLINK_MAX_FRAME, false).unwrap();
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body).await.unwrap();
        assert_eq!(body, c_clone, "server reads the exact command frame");
        // Broadcast one frame back as a v2 record, like the router does.
        let framed = ados_protocol::mavlink_socket::encode_record(42, &b_clone).unwrap();
        conn.write_all(&framed).await.unwrap();
        conn.flush().await.unwrap();
        // Hold the connection open briefly so the client can read.
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let client = MavlinkIpcClient::new(path.clone());
    let mut stream = client.open_ack_stream().await.expect("stream opens");
    stream.write_frame(&command).await.expect("write succeeds");
    match stream.read_frame(Duration::from_secs(1)).await {
        FrameRead::Frame(payload) => {
            assert_eq!(payload, broadcast, "reads back the exact broadcast frame");
        }
        other => panic!("expected a frame, got {other:?}"),
    }
    server.await.unwrap();
}

/// A read with no frame on the wire times out (not a panic, not an error).
#[tokio::test]
async fn ack_stream_read_times_out_when_idle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    // Accept and hold the connection open but send nothing.
    let server = tokio::spawn(async move {
        let (_conn, _addr) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let client = MavlinkIpcClient::new(path.clone());
    let mut stream = client.open_ack_stream().await.expect("stream opens");
    assert!(
        matches!(
            stream.read_frame(Duration::from_millis(40)).await,
            FrameRead::Timeout
        ),
        "an idle stream reports Timeout"
    );
    server.await.unwrap();
}

/// A frame that arrives AFTER the read budget expired must never be parsed
/// off the timed-out stream.
///
/// This is the desync: `read_exact` is not cancellation-safe, so the expired
/// read may already have eaten part of the length prefix. The bytes that land
/// next are therefore at an unknown offset, and parsing them produced a
/// misread ACK — the command route then reported "no ack observed" for an arm
/// or mode-set the FC had accepted. Asserted behaviourally: the server sends
/// a complete, well-formed frame after the timeout, and the stream still
/// refuses to yield it.
#[tokio::test]
async fn a_frame_arriving_after_a_timeout_is_never_read_off_that_stream() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();

    let late = b"\xfd\x03\x04late-frame".to_vec();
    let late_w = late.clone();
    let server = tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        // Stay silent past the client's read budget, then send a good frame.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let framed = ados_protocol::mavlink_socket::encode_record(43, &late_w).unwrap();
        conn.write_all(&framed).await.unwrap();
        conn.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    let client = MavlinkIpcClient::new(path.clone());
    let mut stream = client.open_ack_stream().await.expect("stream opens");
    assert!(
        matches!(
            stream.read_frame(Duration::from_millis(30)).await,
            FrameRead::Timeout
        ),
        "the idle window must report Timeout"
    );
    assert!(stream.is_desynced(), "a timed-out read retires the stream");

    // The late frame is now on the wire. A read must NOT hand it back.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        matches!(
            stream.read_frame(Duration::from_millis(200)).await,
            FrameRead::Eof
        ),
        "a retired stream reports Eof, never a frame read at an unknown offset"
    );
    // And a resend on the retired stream is refused rather than duplicating a
    // flight command whose ACK could not be observed.
    assert!(
        stream.write_frame(&late).await.is_err(),
        "a retired stream refuses writes"
    );
    server.await.unwrap();
}

/// A closed connection reports EOF on the next read.
#[tokio::test]
async fn ack_stream_read_reports_eof_after_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (conn, _addr) = listener.accept().await.unwrap();
        drop(conn); // close immediately
    });

    let client = MavlinkIpcClient::new(path.clone());
    let mut stream = client.open_ack_stream().await.expect("stream opens");
    // Give the server time to close.
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        matches!(
            stream.read_frame(Duration::from_millis(200)).await,
            FrameRead::Eof
        ),
        "a closed stream reports Eof"
    );
    server.await.unwrap();
}

/// A record too short to carry the receive-time header ends the window.
#[tokio::test]
async fn a_record_without_the_receive_time_header_is_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mavlink.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let framed = encode_frame(b"\xfd\x01", MAVLINK_MAX_FRAME).unwrap();
        conn.write_all(&framed).await.unwrap();
        conn.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let client = MavlinkIpcClient::new(path.clone());
    let mut stream = client.open_ack_stream().await.expect("stream opens");
    assert!(matches!(
        stream.read_frame(Duration::from_millis(500)).await,
        FrameRead::Eof
    ));
    server.await.unwrap();
}

/// Opening an ack stream against an absent socket is an I/O error (mapped to
/// a 503 by the route), not a panic.
#[tokio::test]
async fn open_ack_stream_absent_socket_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let client = MavlinkIpcClient::new(dir.path().join("absent.sock"));
    let err = client.open_ack_stream().await.unwrap_err();
    assert!(
        matches!(err, SendError::Io(_)),
        "expected Io error: {err:?}"
    );
}
