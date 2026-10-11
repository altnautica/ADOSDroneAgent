use super::*;
use ados_protocol::framebus::{BoundingBox, Detection};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

fn http_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .use_preconfigured_tls(crate::tls::client_config())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
    )
}

fn paired() -> Option<String> {
    Some("device-key".into())
}

fn unpaired() -> Option<String> {
    None
}

fn relay(has_broker: bool, convex_url: &str, key_source: KeySource) -> PluginPublishRelay {
    PluginPublishRelay::new(
        "dev1".into(),
        has_broker,
        convex_url.into(),
        http_client(),
        key_source,
    )
}

fn batch_json(v: u16) -> Vec<u8> {
    let batch = DetectionBatch {
        v,
        model_id: "offload".into(),
        camera_id: "front".into(),
        frame_id: 3,
        ts_ms: 1_700_000_000_000,
        frame_width: 640,
        frame_height: 480,
        detections: vec![Detection {
            bbox: Some(BoundingBox {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0,
            }),
            class_label: "person".into(),
            confidence: 0.9,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        }],
    };
    serde_json::to_vec(&batch).unwrap()
}

#[test]
fn a_plugin_stream_lands_in_the_plugins_own_subtree_verbatim() {
    let req = CloudPublishRequest::stream("com.example.mapper", "map.pose", vec![1, 2, 3]);
    let msg = stream_message("dev1", &req).unwrap();
    assert_eq!(msg.topic, "ados/dev1/plugin/com.example.mapper/map.pose");
    assert_eq!(msg.payload, vec![1, 2, 3]);
}

#[test]
fn the_detection_stream_lands_on_the_core_vision_topic_as_a_typed_batch() {
    let mut raw: Value = serde_json::from_slice(&batch_json(VISION_DETECTION_VERSION)).unwrap();
    // A key outside the contract is not carried onto the core topic.
    raw["extra"] = json!("dropped");
    let req = CloudPublishRequest::stream(
        "com.example.mapper",
        wire::VISION_DETECTIONS_STREAM,
        serde_json::to_vec(&raw).unwrap(),
    );
    let msg = stream_message("dev1", &req).unwrap();
    assert_eq!(msg.topic, "ados/dev1/vision/detections");
    let out: Value = serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(out["v"], VISION_DETECTION_VERSION);
    assert_eq!(out["detections"][0]["class_label"], "person");
    assert!(out.get("extra").is_none());
}

#[test]
fn the_detection_stream_refuses_anything_but_a_current_batch() {
    for payload in [
        b"not json".to_vec(),
        br#"{"hello":"world"}"#.to_vec(),
        batch_json(VISION_DETECTION_VERSION - 1),
    ] {
        let req = CloudPublishRequest::stream(
            "com.example.mapper",
            wire::VISION_DETECTIONS_STREAM,
            payload,
        );
        assert!(stream_message("dev1", &req).is_err());
    }
}

#[test]
fn a_record_body_names_the_poster_and_defaults_the_subject_to_this_node() {
    let req = CloudPublishRequest::record(
        "com.example.mapper",
        "jobs",
        "session-7",
        br#"{"status":"done","steps":3}"#.to_vec(),
    );
    assert_eq!(
        record_body("dev1", &req).unwrap(),
        json!({
            "posterDeviceId": "dev1",
            "pluginId": "com.example.mapper",
            "collection": "jobs",
            "key": "session-7",
            "data": {"status": "done", "steps": 3},
            "deviceId": "dev1",
        })
    );

    let about_a_drone = req.with_device_id("drone9");
    let body = record_body("dev1", &about_a_drone).unwrap();
    assert_eq!(body["posterDeviceId"], "dev1");
    assert_eq!(body["deviceId"], "drone9");
}

#[test]
fn a_record_payload_must_be_json() {
    let req = CloudPublishRequest::record("com.example.mapper", "jobs", "k", b"{".to_vec());
    assert!(record_body("dev1", &req).is_err());
}

#[test]
fn the_queue_evicts_the_oldest_by_count() {
    let q = StreamQueue::new(2, usize::MAX);
    let msg = |n: u8| StreamMessage {
        topic: "t".into(),
        payload: vec![n],
    };
    assert_eq!(q.push(msg(1)), 0);
    assert_eq!(q.push(msg(2)), 0);
    assert_eq!(q.push(msg(3)), 1);
    assert_eq!(q.dropped_oldest(), 1);
    assert_eq!(q.try_pop().unwrap().payload, vec![2]);
    assert_eq!(q.try_pop().unwrap().payload, vec![3]);
    assert!(q.try_pop().is_none());
}

#[test]
fn the_queue_evicts_the_oldest_by_bytes() {
    // Each message costs 1 (topic) + 10 (payload) = 11 bytes; 25 holds two.
    let q = StreamQueue::new(100, 25);
    let msg = |n: u8| StreamMessage {
        topic: "t".into(),
        payload: vec![n; 10],
    };
    q.push(msg(1));
    q.push(msg(2));
    assert_eq!(q.push(msg(3)), 1);
    assert_eq!(q.len(), 2);
    assert_eq!(q.try_pop().unwrap().payload[0], 2);
    // The byte count follows pops, so room is reclaimed.
    q.push(msg(4));
    assert_eq!(q.len(), 2);
    assert_eq!(q.dropped_oldest(), 1);
}

#[tokio::test]
async fn a_paired_node_with_a_broker_queues_a_stream_and_replies_at_once() {
    let r = relay(true, "", paired);
    let reply = r
        .handle(&CloudPublishRequest::stream(
            "com.example.a",
            "pose",
            b"x".to_vec(),
        ))
        .await;
    assert_eq!(reply, CloudPublishReply::accepted());
    let msg = r.queue().try_pop().unwrap();
    assert_eq!(msg.topic, "ados/dev1/plugin/com.example.a/pose");
}

#[tokio::test]
async fn a_stream_is_refused_with_the_reason_when_it_cannot_leave() {
    let req = CloudPublishRequest::stream("com.example.a", "pose", b"x".to_vec());
    let no_broker = relay(false, "", paired).handle(&req).await;
    assert!(!no_broker.ok);
    assert!(no_broker.error.unwrap().contains("no cloud broker"));

    let not_paired = relay(true, "", unpaired);
    let reply = not_paired.handle(&req).await;
    assert!(!reply.ok);
    assert!(reply.error.unwrap().contains("not paired"));
    assert!(not_paired.queue().is_empty());
}

#[tokio::test]
async fn a_record_is_refused_without_a_cloud_or_a_key() {
    let req = CloudPublishRequest::record("com.example.a", "jobs", "k", b"{}".to_vec());
    assert!(!relay(true, "", paired).handle(&req).await.ok);
    assert!(
        !relay(true, "http://127.0.0.1:9", unpaired)
            .handle(&req)
            .await
            .ok
    );
}

/// A one-shot HTTP server that records the request and answers with
/// `status` and `body`.
async fn one_shot_http(
    status: u16,
    body: &'static str,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let len = text[..head_end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= head_end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        s.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(buf).unwrap()
    });
    (url, server)
}

#[tokio::test]
async fn a_record_posts_the_body_with_the_device_key_and_returns_the_clouds_answer() {
    let (url, server) = one_shot_http(200, r#"{"ok":true}"#).await;
    let r = relay(true, &url, paired);
    let req = CloudPublishRequest::record("com.example.a", "jobs", "k1", br#"{"a":1}"#.to_vec());
    assert_eq!(r.handle(&req).await, CloudPublishReply::accepted());

    let request = server.await.unwrap();
    assert!(request.starts_with("POST /agent/plugin-records "));
    assert!(request
        .to_ascii_lowercase()
        .contains("x-ados-key: device-key"));
    let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body, record_body("dev1", &req).unwrap());
}

#[tokio::test]
async fn a_record_the_cloud_refuses_carries_the_clouds_reason_back() {
    let (url, server) = one_shot_http(
        403,
        r#"{"error":"plugin has no enabled install with cloud.records on this node"}"#,
    )
    .await;
    let r = relay(true, &url, paired);
    let req = CloudPublishRequest::record("com.example.a", "jobs", "k1", b"{}".to_vec());
    let reply = r.handle(&req).await;
    server.await.unwrap();
    assert!(!reply.ok);
    let error = reply.error.unwrap();
    assert!(error.contains("403"), "{error}");
    assert!(error.contains("no enabled install"), "{error}");
    assert_eq!(r.counters.records_failed.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn the_socket_answers_each_request_on_one_connection_and_refuses_an_oversize_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(wire::CLOUD_PUBLISH_SOCK_NAME);
    let listener = bind_command_socket(&path, wire::SOCKET_MODE).unwrap();
    let r = Arc::new(relay(true, "", paired));
    let (_stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(r.clone().serve(listener, stop_rx));

    // The client helper, end to end.
    let reply = wire::send(
        &path,
        &CloudPublishRequest::stream("com.example.a", "pose", b"p".to_vec()),
    )
    .await
    .unwrap();
    assert_eq!(reply, CloudPublishReply::accepted());
    assert_eq!(
        r.queue().try_pop().unwrap().topic,
        "ados/dev1/plugin/com.example.a/pose"
    );

    // Several exchanges on one connection: a readable frame whose body is
    // not a request is refused, and the connection keeps serving.
    let mut s = UnixStream::connect(&path).await.unwrap();
    let ok = CloudPublishRequest::stream("com.example.a", "a", vec![]);
    let garbage = b"not a msgpack request";
    let mut garbage_frame = (garbage.len() as u32).to_be_bytes().to_vec();
    garbage_frame.extend_from_slice(garbage);
    for (frame, want_ok) in [
        (ok.encode().unwrap(), true),
        (garbage_frame, false),
        (ok.encode().unwrap(), true),
    ] {
        s.write_all(&frame).await.unwrap();
        let body = read_length_prefixed(&mut s, wire::MAX_FRAME, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(CloudPublishReply::decode(&body).unwrap().ok, want_ok);
    }

    // A frame header announcing more than the cap is refused and closed
    // without the relay reading (or allocating) the announced body.
    let mut s = UnixStream::connect(&path).await.unwrap();
    s.write_all(&((wire::MAX_FRAME + 1) as u32).to_be_bytes())
        .await
        .unwrap();
    let body = read_length_prefixed(&mut s, wire::MAX_FRAME, true)
        .await
        .unwrap()
        .unwrap();
    let reply = CloudPublishReply::decode(&body).unwrap();
    assert!(!reply.ok);
    assert!(reply.error.unwrap().contains("exceeds"));
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).await.unwrap();
    assert!(
        rest.is_empty(),
        "the connection is closed after the refusal"
    );
}
