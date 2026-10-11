use super::handshake::*;
use super::*;
use crate::host::NoopHost;
use std::time::Duration;

#[cfg(not(target_os = "macos"))]
#[test]
fn wrong_uid_refused() {
    // Another plugin's account, holding a stolen token, is refused.
    assert!(!peer_uid_permitted(Some(61002), Some(61001), 1000));
    // The plugin's own account, root and the daemon's own user pass.
    assert!(peer_uid_permitted(Some(61001), Some(61001), 1000));
    assert!(peer_uid_permitted(Some(0), Some(61001), 1000));
    assert!(peer_uid_permitted(Some(1000), Some(61001), 1000));
    // No plugin account (unsandboxed host): the token alone gates.
    assert!(peer_uid_permitted(Some(61002), None, 1000));
}

#[tokio::test]
async fn stop_plugin_removes_the_socket_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"stop-plugin-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let host = Arc::new(NoopHost);
    let server = PluginIpcServer::new(dir.path(), issuer, bus, host);

    let (path, accept) = server.serve_plugin("com.example.demo").expect("serve");
    // The socket is bound and connectable.
    let mut connected = false;
    for _ in 0..50 {
        if UnixStream::connect(&path).await.is_ok() {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(connected, "expected the bound socket to be connectable");
    assert!(path.exists(), "socket file should exist while served");

    // Stop: the caller aborts the accept task, stop_plugin unlinks the file.
    accept.abort();
    server.stop_plugin("com.example.demo");
    assert!(
        !path.exists(),
        "socket file should be gone after stop_plugin"
    );
}

#[tokio::test]
async fn serving_again_keeps_the_directory_a_running_unit_has_bound() {
    // The unit bind-mounts the plugin's socket directory at start. A daemon
    // restart re-serves the plugin; it must bind the new socket inside the
    // SAME directory, or the running plugin stays bound to a dead one.
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"reserve-secret".to_vec()));
    let server = PluginIpcServer::new(
        dir.path(),
        issuer,
        Arc::new(EventBus::new()),
        Arc::new(NoopHost),
    );
    let (path, accept) = server.serve_plugin("com.example.demo").expect("serve");
    assert_eq!(path, dir.path().join("com.example.demo").join("host.sock"));
    let inode = std::fs::metadata(path.parent().unwrap()).unwrap().ino();
    accept.abort();
    server.stop_plugin("com.example.demo");
    let (again, accept) = server.serve_plugin("com.example.demo").expect("re-serve");
    assert_eq!(
        std::fs::metadata(again.parent().unwrap()).unwrap().ino(),
        inode
    );
    assert!(UnixStream::connect(&again).await.is_ok());
    accept.abort();
}

#[tokio::test]
async fn a_symlink_in_place_of_the_socket_dir_is_replaced_not_followed() {
    // Whatever sits at the plugin's socket-dir path that is not the host's
    // own directory is removed, so the socket (and its chmod/chgrp) never
    // lands where a link points.
    let dir = tempfile::tempdir().expect("tempdir");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, dir.path().join("com.example.demo")).unwrap();
    let server = PluginIpcServer::new(
        dir.path(),
        Arc::new(TokenIssuer::new(b"symlink-secret".to_vec())),
        Arc::new(EventBus::new()),
        Arc::new(NoopHost),
    );
    let (path, accept) = server.serve_plugin("com.example.demo").expect("serve");
    let socket_dir = path.parent().unwrap();
    assert!(!std::fs::symlink_metadata(socket_dir)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    accept.abort();
}

#[tokio::test]
async fn tool_invoke_reaches_a_connected_plugin() {
    use std::collections::BTreeSet;

    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"invoke-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let host = Arc::new(NoopHost);
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus, host);
    let registry = server.invoke_registry();

    let plugin_id = "com.example.demo";
    let (path, accept) = server.serve_plugin(plugin_id).expect("serve");

    // Connect a fake runner once the socket is bound.
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(&path).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (mut rd, mut wr) = stream.expect("connect").into_split();

    // Handshake with an mcp.expose token.
    let mut caps = BTreeSet::new();
    caps.insert("mcp.expose".to_string());
    let token = issuer.mint(plugin_id, &caps, 600).to_token_string();
    let hello = Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: "hello".to_string(),
        capability: String::new(),
        args: Value::Map(vec![]),
        request_id: "h1".to_string(),
        token,
        error: None,
    };
    write_frame(&mut wr, &hello).await.expect("send hello");
    let ready = read_envelope(&mut rd)
        .await
        .expect("read ready")
        .expect("ready envelope");
    assert_eq!(ready.kind, "response");

    // The fake runner answers one tool.invoke by echoing the tool name.
    let runner = tokio::spawn(async move {
        while let Some(env) = read_envelope(&mut rd).await.expect("read") {
            if env.kind == "request" && env.method == "tool.invoke" {
                let tool = match &env.args {
                    Value::Map(pairs) => pairs
                        .iter()
                        .find_map(|(k, v)| {
                            (k.as_str() == Some("tool"))
                                .then(|| v.as_str().unwrap_or("").to_string())
                        })
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let resp = Envelope {
                    version: PROTOCOL_VERSION,
                    kind: "response".to_string(),
                    method: "response".to_string(),
                    capability: String::new(),
                    args: Value::Map(vec![(Value::from("ran"), Value::from(tool))]),
                    request_id: env.request_id.clone(),
                    token: String::new(),
                    error: None,
                };
                write_frame(&mut wr, &resp).await.expect("reply");
            }
        }
    });

    // Wait for the connection to register its invoke sender post-handshake.
    for _ in 0..50 {
        if registry.is_connected(plugin_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        registry.is_connected(plugin_id),
        "connection must register invoke"
    );

    let out = registry
        .invoke(
            plugin_id,
            "greet",
            Value::Map(vec![]),
            Duration::from_secs(2),
        )
        .await
        .expect("invoke result");
    assert_eq!(
        out,
        Value::Map(vec![(Value::from("ran"), Value::from("greet"))])
    );

    runner.abort();
    accept.abort();
    server.stop_plugin(plugin_id);
}

/// A delivery that becomes ready while a request frame is only partly
/// written must not cost the request. The plugin below subscribes to a
/// vehicle event, writes half of a `ping` frame, the host publishes the
/// event, then the plugin writes the rest: both the event and the ping's
/// response must arrive, and the session must stay in frame.
#[tokio::test]
async fn a_delivery_racing_a_partly_written_request_does_not_desync_the_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let issuer = Arc::new(TokenIssuer::new(b"race-secret".to_vec()));
    let bus = Arc::new(EventBus::new());
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), bus.clone(), Arc::new(NoopHost));
    let plugin_id = "com.example.demo";
    let (path, accept) = server.serve_plugin(plugin_id).expect("serve");
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(&path).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (mut rd, mut wr) = stream.expect("connect").into_split();
    let token = issuer
        .mint(
            plugin_id,
            &std::collections::BTreeSet::from(["event.subscribe".to_string()]),
            600,
        )
        .to_token_string();
    let request = |method: &str, id: &str, args: Value| Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: method.to_string(),
        capability: String::new(),
        args,
        request_id: id.to_string(),
        token: token.clone(),
        error: None,
    };
    write_frame(&mut wr, &request("hello", "h1", Value::Map(vec![])))
        .await
        .unwrap();
    read_envelope(&mut rd).await.unwrap().expect("ready");
    write_frame(
        &mut wr,
        &request(
            "event.subscribe",
            "s1",
            Value::Map(vec![(Value::from("topic"), Value::from("vehicle.armed"))]),
        ),
    )
    .await
    .unwrap();
    let subscribed = read_envelope(&mut rd).await.unwrap().expect("subscribed");
    assert!(subscribed.error.is_none(), "{:?}", subscribed.error);

    let ping = request("ping", "p1", Value::Map(vec![]))
        .encode_frame()
        .unwrap();
    let (head, tail) = ping.split_at(ping.len() / 2);
    wr.write_all(head).await.unwrap();
    wr.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(Event {
        topic: "vehicle.armed".to_string(),
        timestamp_ms: now_ms(),
        publisher_plugin_id: crate::vehicle_events::HOST_PUBLISHER.to_string(),
        payload: Value::Map(vec![]),
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    wr.write_all(tail).await.unwrap();
    wr.flush().await.unwrap();

    let mut got_event = false;
    let mut got_pong = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !(got_event && got_pong) {
        let env = tokio::time::timeout_at(deadline, read_envelope(&mut rd))
            .await
            .expect("the ping response never arrived: the session lost its framing")
            .expect("a well-formed frame")
            .expect("the session stayed open");
        if env.kind == "event" {
            got_event = true;
        } else if env.request_id == "p1" {
            assert!(env.error.is_none(), "{:?}", env.error);
            got_pong = true;
        }
    }
    accept.abort();
    server.stop_plugin(plugin_id);
}

fn seq_of(snapshot: &Value) -> i64 {
    snapshot
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("seq")))
        .and_then(|(_, v)| v.as_i64())
        .expect("seq")
}

fn snapshot(seq: i64) -> Arc<Value> {
    Arc::new(Value::Map(vec![(Value::from("seq"), Value::from(seq))]))
}

/// A producer faster than the cap reaches the plugin no faster than the
/// cap, and what does reach it is the newest state, never a queued stale
/// one: the final delivery is the final snapshot.
#[tokio::test(start_paused = true)]
async fn telemetry_pacing_caps_the_rate_and_delivers_the_newest_state() {
    let (fanout, rx) = broadcast::channel(8);
    let (tx, mut paced) = mpsc::channel(1);
    let pacer = tokio::spawn(pace_snapshots(rx, tx, TELEMETRY_STATE_MIN_INTERVAL));
    // 100 Hz for one second: ten times the cap.
    let producer = tokio::spawn(async move {
        for seq in 0..100 {
            fanout.send(snapshot(seq)).unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    let mut arrivals = Vec::new();
    while let Some(s) = paced.recv().await {
        arrivals.push((tokio::time::Instant::now(), seq_of(&s)));
    }
    producer.await.unwrap();
    pacer.await.unwrap();

    assert_eq!(
        arrivals.first().map(|a| a.1),
        Some(0),
        "the first goes at once"
    );
    assert_eq!(arrivals.last().map(|a| a.1), Some(99), "newest wins");
    for pair in arrivals.windows(2) {
        assert!(
            pair[1].0 - pair[0].0 >= TELEMETRY_STATE_MIN_INTERVAL,
            "{} and {} came {:?} apart",
            pair[0].1,
            pair[1].1,
            pair[1].0 - pair[0].0
        );
        assert!(pair[1].1 > pair[0].1, "a snapshot went backwards");
    }
    assert!(arrivals.len() <= 11, "{} deliveries in 1 s", arrivals.len());
}
