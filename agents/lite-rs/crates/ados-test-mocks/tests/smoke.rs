//! Smoke tests for the in-process MQTT broker and minimal RTSP
//! server fixtures. Both tests exercise the full happy path on a
//! loopback ephemeral port — no external network reachability is
//! required.

use std::time::Duration;

use ados_test_mocks::{MockMqttBroker, MockRtspServer};
use rumqttc::{AsyncClient, MqttOptions, Packet, QoS};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mqtt_broker_round_trip() {
    let broker = MockMqttBroker::start().await.expect("broker start");
    let port = broker.port();

    let mut opts = MqttOptions::new("mock-test-client", "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));

    let (client, mut event_loop) = AsyncClient::new(opts, 16);

    client
        .subscribe("ados/test/topic", QoS::AtMostOnce)
        .await
        .expect("subscribe");

    // Drive the event loop so SUBSCRIBE flushes; the broker echoes a
    // SUBACK that we wait on before publishing.
    let driver = tokio::spawn(async move {
        let mut payload = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                ev = event_loop.poll() => {
                    match ev {
                        Ok(rumqttc::Event::Incoming(Packet::Publish(p))) => {
                            payload = Some(p.payload.to_vec());
                            break;
                        }
                        Ok(_) => continue,
                        Err(err) => {
                            // Ignore the connection-closed error
                            // raised when the test drops the client
                            // at the end of the run.
                            tracing::debug!(?err, "event loop ended");
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        payload
    });

    // Give the SUBSCRIBE a moment to round-trip before publishing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    client
        .publish(
            "ados/test/topic",
            QoS::AtMostOnce,
            false,
            b"hello-fixture".to_vec(),
        )
        .await
        .expect("publish");

    let payload = driver.await.expect("driver join").expect("payload");
    assert_eq!(payload, b"hello-fixture");

    broker.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtsp_server_captures_one_interleaved_frame() {
    let server = MockRtspServer::start().await.expect("rtsp start");
    let port = server.port();

    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("rtsp connect");

    let stream_url = format!("rtsp://127.0.0.1:{}/test", port);

    let announce = format!(
        "ANNOUNCE {url} RTSP/1.0\r\n\
         CSeq: 1\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: 0\r\n\
         \r\n",
        url = stream_url
    );
    stream.write_all(announce.as_bytes()).await.expect("write announce");

    let setup = format!(
        "SETUP {url}/streamid=0 RTSP/1.0\r\n\
         CSeq: 2\r\n\
         Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\
         \r\n",
        url = stream_url
    );
    stream.write_all(setup.as_bytes()).await.expect("write setup");

    let record = format!(
        "RECORD {url} RTSP/1.0\r\n\
         CSeq: 3\r\n\
         Session: 00000001\r\n\
         Range: npt=0.000-\r\n\
         \r\n",
        url = stream_url
    );
    stream.write_all(record.as_bytes()).await.expect("write record");

    // Drain server responses out of the socket so the parser does
    // not stall on flow control. We do not assert on the response
    // bytes here; the smoke is the capture count.
    drain_briefly(&mut stream).await;

    // One interleaved RTP frame: `$`, channel byte 0, 2-byte big
    // endian length, then the payload. The 12-byte payload is a
    // syntactically minimal RTP header (V=2, no payload data).
    let payload: [u8; 12] = [
        0x80, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xde, 0xad, 0xbe, 0xef,
    ];
    let mut frame = vec![b'$', 0u8];
    frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    frame.extend_from_slice(&payload);
    stream.write_all(&frame).await.expect("write rtp frame");

    let teardown = format!(
        "TEARDOWN {url} RTSP/1.0\r\n\
         CSeq: 4\r\n\
         Session: 00000001\r\n\
         \r\n",
        url = stream_url
    );
    stream.write_all(teardown.as_bytes()).await.expect("write teardown");

    // Give the server a moment to drain the TCP buffer into the
    // capture vector before we snapshot it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if !server.captured_rtp_packets().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let captured = server.captured_rtp_packets();
    assert_eq!(captured.len(), 1, "expected exactly one captured RTP frame");
    assert_eq!(captured[0], payload, "captured payload mismatch");

    let sessions = server.captured_session_count();
    assert_eq!(sessions, 1, "expected exactly one minted session");

    drop(stream);
    server.shutdown().await;
}

async fn drain_briefly(stream: &mut TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    loop {
        let read = tokio::time::timeout(Duration::from_millis(50), stream.read(&mut buf)).await;
        match read {
            Ok(Ok(0)) => return,
            Ok(Ok(_n)) => {
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
            }
            Ok(Err(_)) => return,
            Err(_) => return,
        }
    }
}
