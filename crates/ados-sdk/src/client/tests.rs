use super::*;
use std::sync::atomic::AtomicUsize;

#[test]
fn driver_kinds_map_to_catalog_caps() {
    for kind in [
        "camera",
        "lidar",
        "gimbal",
        "gps",
        "esc",
        "payload-actuator",
    ] {
        let cap = driver_kind_capability(kind).expect(kind);
        assert!(
            ados_protocol::capabilities::is_known_agent_capability(cap),
            "{kind} maps to {cap}, which is not in the catalog"
        );
    }
    assert_eq!(driver_kind_capability("sonar"), None);
}

#[tokio::test]
async fn driver_kind_unknown_is_refused_before_sending() {
    // Not connected: a send would answer NotConnected, so an Rpc error
    // proves the refusal came first.
    let client = PluginIpcClient::new("com.example.demo", "tok", "/run/ados/plugins/x.sock");
    let err = client
        .peripheral_register_driver("sonar", "ref-1")
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::Rpc(m) if m.contains("sonar")));
}

fn batch_for(camera_id: &str) -> ados_protocol::framebus::DetectionBatch {
    ados_protocol::framebus::DetectionBatch {
        v: ados_protocol::framebus::VISION_DETECTION_VERSION,
        model_id: "m".to_string(),
        camera_id: camera_id.to_string(),
        frame_id: 1,
        ts_ms: 0,
        frame_width: 640,
        frame_height: 480,
        detections: Vec::new(),
    }
}

fn event_envelope(method: &str, args: Value) -> Envelope {
    Envelope {
        version: ados_protocol::plugin::PROTOCOL_VERSION,
        kind: "event".to_string(),
        method: method.to_string(),
        capability: String::new(),
        args,
        request_id: "e1".to_string(),
        token: String::new(),
        error: None,
    }
}

#[test]
fn a_pushed_detection_batch_reaches_a_subscribed_callback() {
    // The regression this whole arm exists for. The deliver envelope carries
    // NO `topic`, so before the DELIVER_DETECTION arm existed every batch
    // fell through to dispatch_event, failed the topic lookup and was
    // dropped silently — a Rust plugin could subscribe and never be told
    // anything was wrong.
    let map: CallbackMap = Arc::new(Mutex::new(HashMap::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    register_callback(
        &map,
        VISION_ANY_CAMERA,
        Arc::new(move |_args| {
            seen.fetch_add(1, Ordering::Relaxed);
        }),
    );

    let batch = batch_for("main");
    let env = event_envelope(
        ados_protocol::framebus::methods::DELIVER_DETECTION,
        Value::Map(vec![(
            Value::from("batch"),
            Value::Binary(batch.to_msgpack().unwrap()),
        )]),
    );

    dispatch_detection(&map, &env);
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "the batch must be delivered"
    );
}

#[test]
fn a_detection_for_another_camera_does_not_wake_a_filtered_subscriber() {
    let map: CallbackMap = Arc::new(Mutex::new(HashMap::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    register_callback(
        &map,
        "left",
        Arc::new(move |_args| {
            seen.fetch_add(1, Ordering::Relaxed);
        }),
    );

    let batch = batch_for("right");
    let env = event_envelope(
        ados_protocol::framebus::methods::DELIVER_DETECTION,
        Value::Map(vec![(
            Value::from("batch"),
            Value::Binary(batch.to_msgpack().unwrap()),
        )]),
    );

    dispatch_detection(&map, &env);
    assert_eq!(hits.load(Ordering::Relaxed), 0);
}

#[test]
fn an_undecodable_batch_is_dropped_rather_than_panicking() {
    let map: CallbackMap = Arc::new(Mutex::new(HashMap::new()));
    register_callback(&map, VISION_ANY_CAMERA, Arc::new(|_| unreachable!()));
    let env = event_envelope(
        ados_protocol::framebus::methods::DELIVER_DETECTION,
        Value::Map(vec![(
            Value::from("batch"),
            Value::Binary(vec![0xff, 0x00, 0x13]),
        )]),
    );
    dispatch_detection(&map, &env); // must not panic, must not deliver
}

#[test]
fn a_pushed_button_press_reaches_every_subscriber() {
    // A press has no subject to filter on, so both subscribers fire.
    let map: CallbackMap = Arc::new(Mutex::new(HashMap::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    for key in ["a", "b"] {
        let seen = hits.clone();
        register_callback(
            &map,
            key,
            Arc::new(move |_args| {
                seen.fetch_add(1, Ordering::Relaxed);
            }),
        );
    }
    let env = event_envelope(
        ados_protocol::buttons::DELIVER,
        Value::Map(vec![(
            Value::from("press"),
            Value::Map(vec![
                (Value::from("pin"), Value::Integer(5.into())),
                (Value::from("kind"), Value::from("short")),
            ]),
        )]),
    );
    dispatch_button(&map, &env);
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}

#[test]
fn request_ids_increment_and_are_r_prefixed() {
    let client = PluginIpcClient::new("com.example.demo", "tok", "/run/ados/plugins/x.sock");
    // The id stream is `r1`, `r2`, ... exactly like the Python `_next_id`.
    let a = client.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let b = client.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    assert_eq!(format!("r{a}"), "r1");
    assert_eq!(format!("r{b}"), "r2");
}

#[test]
fn fnmatch_routes_topics_like_the_host() {
    assert!(fnmatch("mavlink.*", "mavlink.heartbeat"));
    assert!(fnmatch("plugin.demo.*", "plugin.demo.metric"));
    assert!(fnmatch("HEARTBEAT", "HEARTBEAT"));
    assert!(!fnmatch("mavlink.*", "mavlinkx"));
    assert!(!fnmatch("vehicle.armed", "vehicle.disarmed"));
}

#[test]
fn callback_map_dispatches_to_exact_and_glob_matches() {
    use std::sync::atomic::{AtomicUsize, Ordering as O};
    let map: CallbackMap = Arc::new(Mutex::new(HashMap::new()));
    let exact = Arc::new(AtomicUsize::new(0));
    let glob = Arc::new(AtomicUsize::new(0));
    let e = exact.clone();
    let g = glob.clone();
    register_callback(
        &map,
        "HEARTBEAT",
        Arc::new(move |_| {
            e.fetch_add(1, O::Relaxed);
        }),
    );
    register_callback(
        &map,
        "HEART*",
        Arc::new(move |_| {
            g.fetch_add(1, O::Relaxed);
        }),
    );
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "event".into(),
        method: "mavlink.deliver".into(),
        capability: String::new(),
        args: Value::Map(vec![(Value::from("msg_name"), Value::from("HEARTBEAT"))]),
        request_id: "evt".into(),
        token: String::new(),
        error: None,
    };
    dispatch_mavlink(&map, &env);
    assert_eq!(exact.load(O::Relaxed), 1);
    assert_eq!(glob.load(O::Relaxed), 1);
}

#[test]
fn vision_deliver_routes_by_camera_id_and_wildcard() {
    use ados_protocol::framebus::{FrameDescriptor, FrameFormat, FRAMEBUS_DESCRIPTOR_VERSION};
    use std::sync::atomic::{AtomicUsize, Ordering as O};
    let map: CallbackMap = Arc::new(Mutex::new(HashMap::new()));
    let exact = Arc::new(AtomicUsize::new(0));
    let any = Arc::new(AtomicUsize::new(0));
    let other = Arc::new(AtomicUsize::new(0));
    let e = exact.clone();
    let a = any.clone();
    let o = other.clone();
    register_callback(
        &map,
        "uvc-0",
        Arc::new(move |_| {
            e.fetch_add(1, O::Relaxed);
        }),
    );
    register_callback(
        &map,
        VISION_ANY_CAMERA,
        Arc::new(move |_| {
            a.fetch_add(1, O::Relaxed);
        }),
    );
    register_callback(
        &map,
        "uvc-1",
        Arc::new(move |_| {
            o.fetch_add(1, O::Relaxed);
        }),
    );

    let descriptor = FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "uvc-0".into(),
        frame_id: 1,
        ts_ns: 0,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: ados_protocol::framebus::TIMEBASE_HOST,
        width: 4,
        height: 4,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-uvc-0".into(),
        slot: 0,
        seq: 1,
        byte_len: 48,
    };
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "event".into(),
        method: ados_protocol::framebus::methods::DELIVER_FRAME.into(),
        capability: "vision.frame.read".into(),
        args: Value::Map(vec![(
            Value::from("descriptor"),
            Value::Binary(descriptor.to_msgpack().unwrap()),
        )]),
        request_id: "vis-1".into(),
        token: String::new(),
        error: None,
    };
    dispatch_vision(&map, &env);
    // The exact camera key and the wildcard both fire; the other camera does not.
    assert_eq!(exact.load(O::Relaxed), 1);
    assert_eq!(any.load(O::Relaxed), 1);
    assert_eq!(other.load(O::Relaxed), 0);
}

#[test]
fn error_string_maps_to_typed_variants() {
    assert!(matches!(
        map_error_string("capability_denied: mission.read"),
        ClientError::CapabilityDenied(c) if c == "mission.read"
    ));
    assert!(matches!(
        map_error_string("allowlist_violation: ffmpeg"),
        ClientError::AllowlistViolation(b) if b == "ffmpeg"
    ));
    assert!(matches!(
        map_error_string("publish not permitted on topic mavlink.x"),
        ClientError::CapabilityDenied(_)
    ));
    assert!(matches!(
        map_error_string("something else"),
        ClientError::Rpc(m) if m == "something else"
    ));
}

#[test]
fn map_readers_extract_fields() {
    let args = Value::Map(vec![
        (Value::from("topic"), Value::from("plugin.demo.x")),
        (Value::from("delivered"), Value::from(3i64)),
    ]);
    assert_eq!(map_get_str(&args, "topic"), Some("plugin.demo.x"));
    assert_eq!(map_get_i64(&args, "delivered"), Some(3));
    assert!(map_get(&args, "missing").is_none());
}

fn token_string(caps: &[&str], expires_at: i64) -> String {
    CapabilityToken {
        plugin_id: "p".to_string(),
        session_id: "s".to_string(),
        granted_caps: caps.iter().map(|c| (*c).to_string()).collect(),
        issued_at: 0,
        expires_at,
        signature: "00".repeat(32),
    }
    .to_token_string()
}

fn response(request_id: &str) -> Envelope {
    Envelope {
        version: ados_protocol::plugin::PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: String::new(),
        capability: String::new(),
        args: Value::Map(vec![(Value::from("ready"), Value::Boolean(true))]),
        request_id: request_id.to_string(),
        token: String::new(),
        error: None,
    }
}

#[tokio::test]
async fn a_rotated_token_is_adopted_and_presented_on_later_requests() {
    // The host re-mints on a permission change and before the TTL, then
    // gates the live session on ITS copy — so ignoring the push is free
    // until the socket breaks, at which point `hello` presents a token the
    // issuer has rotated past and the plugin is refused for the rest of the
    // flight. The observable contract is therefore what the client puts in
    // the `token` field after the push.
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("p.sock");
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind");

    let first = token_string(&["event.publish", "mission.read"], 100);
    let rotated = token_string(&["event.publish"], 900);

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let pushed = rotated.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (mut r, mut w) = stream.into_split();
        let hello = read_envelope(&mut r)
            .await
            .expect("hello read")
            .expect("hello frame");
        recorded.lock().expect("seen").push(hello.token.clone());
        w.write_all(&response(&hello.request_id).encode_frame().unwrap())
            .await
            .expect("ready");
        let refresh = event_envelope(
            TOKEN_REFRESH_METHOD,
            Value::Map(vec![
                (Value::from("token"), Value::from(pushed.as_str())),
                (Value::from("expires_at"), Value::from(900i64)),
                (
                    Value::from("granted_caps"),
                    Value::Array(vec![Value::from("event.publish")]),
                ),
            ]),
        );
        w.write_all(&refresh.encode_frame().unwrap())
            .await
            .expect("push refresh");
        while let Ok(Some(req)) = read_envelope(&mut r).await {
            recorded.lock().expect("seen").push(req.token.clone());
            if w.write_all(&response(&req.request_id).encode_frame().unwrap())
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let client = PluginIpcClient::new("p", first.as_str(), &sock);
    client.connect().await.expect("connect");

    // The push and the next request race on the reader task, so poll rather
    // than sleep a fixed amount: bounded, so a dropped push fails the test
    // instead of hanging it.
    let mut adopted = false;
    for _ in 0..50 {
        client.ping().await.expect("ping");
        if seen.lock().expect("seen").last() == Some(&rotated) {
            adopted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let tokens = seen.lock().expect("seen").clone();
    client.close().await;
    server.abort();

    assert_eq!(
        tokens.first().map(String::as_str),
        Some(first.as_str()),
        "the handshake presents the token the process was given"
    );
    assert!(
        adopted,
        "the rotated token must be presented after the push, saw {tokens:?}"
    );
}
