use super::*;
use ados_protocol::frame::encode_frame;
use ados_protocol::framebus::FrameDescriptor;
use ados_protocol::framebus::FrameFormat;
use ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION;
use ados_protocol::ipc::IpcBroadcast;

fn temp_sock(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ados-visclient-test-{}-{}.sock",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// A client on a fast reconnect interval, once its first connection is up.
async fn connected_client(path: &Path) -> VisionClient {
    let client = VisionClient::spawn_with_interval(path.to_path_buf(), Duration::from_millis(50));
    assert!(client.connected_within(Duration::from_secs(2)).await);
    client
}

/// A fake engine that answers every host request with an empty success
/// response and reports each request's method on the returned channel.
fn answering_engine(
    server: std::sync::Arc<IpcBroadcast>,
    mut inbound: tokio::sync::mpsc::Receiver<ados_protocol::ipc::InboundCommand>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let engine = tokio::spawn(async move {
        while let Some(cmd) = inbound.recv().await {
            let req = Envelope::from_msgpack(&cmd.payload).expect("a request envelope");
            let _ = seen_tx.send(req.method.clone());
            server
                .broadcast(response_envelope(Value::Map(vec![])).into())
                .await;
        }
    });
    (engine, seen_rx)
}

fn sample_descriptor(frame_id: u64) -> FrameDescriptor {
    FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "uvc-0".into(),
        frame_id,
        ts_ns: 1,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: ados_protocol::framebus::TIMEBASE_HOST,
        width: 64,
        height: 48,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 0,
        seq: frame_id,
        byte_len: (64 * 48 * 3) as u32,
    }
}

async fn next_method(seen: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(2), seen.recv())
        .await
        .expect("a host request within timeout")
        .expect("engine task alive")
}

#[tokio::test]
async fn an_engine_that_starts_after_the_host_is_picked_up() {
    let path = temp_sock("late");
    let client = VisionClient::spawn_with_interval(path.clone(), Duration::from_millis(50));
    let mut rx = client.subscribe_frames();

    // No engine yet: a request is a transient refusal, and a plugin
    // subscribe records the want instead of failing it.
    let err = client
        .register_model(&Value::Map(vec![]))
        .await
        .unwrap_err();
    assert_eq!(err, VisionRpcError(VISION_ENGINE_UNAVAILABLE.to_string()));
    client.arm_frame_push().await;

    let (server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let server = std::sync::Arc::new(server);
    let (_engine, mut seen) = answering_engine(server.clone(), inbound.unwrap());
    assert!(client.connected_within(Duration::from_secs(2)).await);

    // The subscribe made while down is armed on the new connection.
    assert_eq!(next_method(&mut seen).await, methods::SUBSCRIBE_FRAMES);
    let descriptor = sample_descriptor(1);
    server
        .broadcast(deliver_envelope(&descriptor.to_msgpack().unwrap()).into())
        .await;
    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("descriptor within timeout")
        .expect("descriptor, not lagged/closed");
    assert_eq!(FrameDescriptor::from_msgpack(&got).unwrap(), descriptor);

    client.register_model(&Value::Map(vec![])).await.unwrap();
    assert_eq!(next_method(&mut seen).await, methods::REGISTER_MODEL);
}

#[tokio::test]
async fn an_engine_restart_reconnects_and_rearms_the_same_subscriber() {
    let path = temp_sock("restart");
    let (first, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let first = std::sync::Arc::new(first);
    let (first_engine, mut seen) = answering_engine(first.clone(), inbound.unwrap());
    let client = connected_client(&path).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut rx = client.subscribe_frames();
    client.arm_frame_push().await;
    assert_eq!(next_method(&mut seen).await, methods::SUBSCRIBE_FRAMES);

    // The engine goes away (its answering task holds the other handle).
    first_engine.abort();
    let _ = first_engine.await;
    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while client.is_connected() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!client.is_connected(), "the lost engine must be noticed");
    let err = client.infer(&Value::Map(vec![])).await.unwrap_err();
    assert_eq!(err, VisionRpcError(VISION_ENGINE_UNAVAILABLE.to_string()));

    // It comes back: the client reconnects and re-arms the push by itself,
    // and the plugin's original receiver keeps receiving.
    let (second, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let second = std::sync::Arc::new(second);
    let (_engine, mut seen) = answering_engine(second.clone(), inbound.unwrap());
    assert!(client.connected_within(Duration::from_secs(2)).await);
    assert_eq!(next_method(&mut seen).await, methods::SUBSCRIBE_FRAMES);

    let descriptor = sample_descriptor(2);
    second
        .broadcast(deliver_envelope(&descriptor.to_msgpack().unwrap()).into())
        .await;
    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("descriptor within timeout")
        .expect("descriptor, not lagged/closed");
    assert_eq!(FrameDescriptor::from_msgpack(&got).unwrap(), descriptor);
}

fn deliver_envelope(descriptor: &[u8]) -> Vec<u8> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "event".to_string(),
        method: methods::DELIVER_FRAME.to_string(),
        capability: "vision.frame.read".to_string(),
        args: Value::Map(vec![(
            Value::from("descriptor"),
            Value::Binary(descriptor.to_vec()),
        )]),
        request_id: "vis-frame-1".to_string(),
        token: String::new(),
        error: None,
    };
    let body = env.to_msgpack().unwrap();
    encode_frame(&body, PLUGIN_MAX_FRAME).unwrap()
}

fn response_envelope(request_kind: Value) -> Vec<u8> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: request_kind,
        request_id: "vis-1".to_string(),
        token: String::new(),
        error: None,
    };
    let body = env.to_msgpack().unwrap();
    encode_frame(&body, PLUGIN_MAX_FRAME).unwrap()
}

#[tokio::test]
async fn frame_descriptors_fan_out_to_a_subscriber() {
    let path = temp_sock("fanout");
    // The engine side: a broadcast socket the host connects to. The plugin
    // contract rejects zero-length frames, so reject_zero is true.
    let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

    let client = connected_client(&path).await;
    let mut rx = client.subscribe_frames();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let descriptor = FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "uvc-0".into(),
        frame_id: 1,
        ts_ns: 1,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: ados_protocol::framebus::TIMEBASE_HOST,
        width: 64,
        height: 48,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 0,
        seq: 1,
        byte_len: (64 * 48 * 3) as u32,
    };
    let bytes = descriptor.to_msgpack().unwrap();
    server.broadcast(deliver_envelope(&bytes).into()).await;

    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("descriptor within timeout")
        .expect("descriptor, not lagged/closed");
    assert_eq!(FrameDescriptor::from_msgpack(&got).unwrap(), descriptor);
}

fn deliver_detection_envelope(batch: &[u8]) -> Vec<u8> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "event".to_string(),
        method: methods::DELIVER_DETECTION.to_string(),
        capability: "vision.detection.subscribe".to_string(),
        args: Value::Map(vec![(Value::from("batch"), Value::Binary(batch.to_vec()))]),
        request_id: "vis-det-uvc-0-1".to_string(),
        token: String::new(),
        error: None,
    };
    let body = env.to_msgpack().unwrap();
    encode_frame(&body, PLUGIN_MAX_FRAME).unwrap()
}

#[tokio::test]
async fn detection_batches_fan_out_to_a_subscriber() {
    use ados_protocol::framebus::{DetectionBatch, VISION_DETECTION_VERSION};
    let path = temp_sock("det-fanout");
    let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

    let client = connected_client(&path).await;
    let mut rx = client.subscribe_detections();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let batch = DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "m".into(),
        camera_id: "uvc-0".into(),
        frame_id: 1,
        ts_ms: 1,
        frame_width: 640,
        frame_height: 480,
        detections: vec![],
    };
    let bytes = batch.to_msgpack().unwrap();
    server
        .broadcast(deliver_detection_envelope(&bytes).into())
        .await;

    let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("batch within timeout")
        .expect("batch, not lagged/closed");
    assert_eq!(DetectionBatch::from_msgpack(&got).unwrap(), batch);
}

#[tokio::test]
async fn request_returns_the_engine_response_args() {
    let path = temp_sock("request");
    let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

    let client = connected_client(&path).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The engine answers the next request with a fixed response.
    let result = Value::Map(vec![(Value::from("registered"), Value::Boolean(true))]);
    server
        .broadcast(response_envelope(result.clone()).into())
        .await;

    let args = Value::Map(vec![(Value::from("model_id"), Value::from("m1"))]);
    let got = client.register_model(&args).await.unwrap();
    assert_eq!(got, result);
}

#[tokio::test]
async fn engine_error_surfaces_as_rpc_error() {
    let path = temp_sock("error");
    let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

    let client = connected_client(&path).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: "vis-1".to_string(),
        token: String::new(),
        error: Some("model not found".to_string()),
    };
    let body = env.to_msgpack().unwrap();
    server
        .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap().into())
        .await;

    let args = Value::Map(vec![(Value::from("model_id"), Value::from("missing"))]);
    let err = client.infer(&args).await.unwrap_err();
    assert_eq!(err, VisionRpcError("model not found".to_string()));
}

#[tokio::test]
async fn a_refused_arm_is_retried_by_the_concurrent_second_subscriber() {
    let path = temp_sock("arm-race");
    // `Some(16)` hands the fake engine the host's own requests, so the test
    // can count how many subscribe attempts actually reached the wire.
    let (server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");
    let server = std::sync::Arc::new(server);

    let client = connected_client(&path).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The engine refuses the first subscribe and accepts the second.
    let engine_server = server.clone();
    let engine = tokio::spawn(async move {
        let mut seen = Vec::new();
        for attempt in 0..2 {
            let cmd = inbound.recv().await.expect("a host request");
            let req = Envelope::from_msgpack(&cmd.payload).expect("a request envelope");
            seen.push(req.method.clone());
            let reply = Envelope {
                version: PROTOCOL_VERSION,
                kind: "response".to_string(),
                method: "response".to_string(),
                capability: String::new(),
                args: Value::Map(vec![]),
                request_id: req.request_id,
                token: String::new(),
                error: (attempt == 0).then(|| "engine busy".to_string()),
            };
            let body = reply.to_msgpack().unwrap();
            engine_server
                .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap().into())
                .await;
        }
        seen
    });

    // Two plugins subscribing at host startup, the normal case. A
    // test-and-set flag let the second one return on the first one's
    // in-flight `true` and then be stranded when that attempt failed.
    tokio::join!(client.arm_frame_push(), client.arm_frame_push());

    let seen = tokio::time::timeout(Duration::from_secs(2), engine)
        .await
        .expect("the engine answered both attempts")
        .unwrap();
    assert_eq!(
        seen,
        vec![
            methods::SUBSCRIBE_FRAMES.to_string(),
            methods::SUBSCRIBE_FRAMES.to_string()
        ],
        "the refused arm must be retried by the second subscriber"
    );

    // Armed now, so a third subscriber returns without another round-trip —
    // there is no engine task left to answer one.
    tokio::time::timeout(Duration::from_millis(500), client.arm_frame_push())
        .await
        .expect("an armed connection does not re-request");
}

#[tokio::test]
async fn a_frame_push_with_no_subscriber_is_dropped_before_the_decode() {
    let path = temp_sock("idle-fanout");
    let (server, _inbound) = IpcBroadcast::bind(&path, 256, false, None).await.unwrap();

    let client = connected_client(&path).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A push arriving while no plugin holds a receiver, then a response.
    // The reader must stay on the wire: dropping the push must not consume
    // or reorder the response behind it.
    server
        .broadcast(deliver_envelope(b"not-a-descriptor").into())
        .await;
    let result = Value::Map(vec![(Value::from("registered"), Value::Boolean(true))]);
    server
        .broadcast(response_envelope(result.clone()).into())
        .await;

    let args = Value::Map(vec![(Value::from("model_id"), Value::from("m1"))]);
    let got = client.register_model(&args).await.unwrap();
    assert_eq!(got, result);
}
