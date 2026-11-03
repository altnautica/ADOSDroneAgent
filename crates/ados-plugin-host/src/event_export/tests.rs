use super::*;
use rmpv::Value;
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn event(topic: &str, publisher: &str, n: i64) -> Event {
    Event {
        topic: topic.to_string(),
        timestamp_ms: 1_000 + n,
        publisher_plugin_id: publisher.to_string(),
        payload: Value::Map(vec![(Value::from("n"), Value::from(n))]),
    }
}

async fn connect(path: &Path, request: &str) -> tokio::io::Lines<BufReader<UnixStream>> {
    let mut stream = UnixStream::connect(path).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    BufReader::new(stream).lines()
}

async fn next_json(lines: &mut tokio::io::Lines<BufReader<UnixStream>>) -> serde_json::Value {
    let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .expect("a line within the deadline")
        .unwrap()
        .expect("the socket is still open");
    serde_json::from_str(&line).unwrap()
}

#[test]
fn a_request_needs_the_version_the_op_and_well_formed_topics() {
    let ok = parse_request(br#"{"v":1,"op":"subscribe","topics":["a.b","c"]}"#).unwrap();
    assert_eq!(ok.len(), 2);
    assert!(parse_request(br#"{"v":2,"op":"subscribe","topics":["a"]}"#).is_err());
    assert!(parse_request(br#"{"v":1,"op":"tail","topics":["a"]}"#).is_err());
    assert!(parse_request(br#"{"v":1,"op":"subscribe","topics":[]}"#).is_err());
    assert!(parse_request(br#"{"v":1,"op":"subscribe","topics":["has space"]}"#).is_err());
    let many: Vec<String> = (0..=EXPORT_MAX_TOPICS).map(|i| format!("t{i}")).collect();
    let body = json!({"v": 1, "op": "subscribe", "topics": many}).to_string();
    assert!(parse_request(body.as_bytes()).is_err());
    assert!(parse_request(b"not json").is_err());
}

#[test]
fn an_event_line_carries_the_host_stamped_publisher_and_json_payload() {
    let line = event_line(&event("follow.state", "com.example.follow", 7));
    assert_eq!(line.last(), Some(&b'\n'));
    let v: serde_json::Value = serde_json::from_slice(&line).unwrap();
    assert_eq!(v["v"], 1);
    assert_eq!(v["topic"], "follow.state");
    assert_eq!(v["publisher"], "com.example.follow");
    assert_eq!(v["ts_ms"], 1_007);
    assert_eq!(v["payload"]["n"], 7);
}

#[tokio::test]
async fn forwards_only_the_subscribed_topics() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new());
    let (path, task) = serve_event_export(bus.clone(), dir.path().join("plugin-host")).unwrap();

    let mut lines = connect(&path, r#"{"v":1,"op":"subscribe","topics":["nav.state"]}"#).await;
    assert_eq!(next_json(&mut lines).await["ok"], true);

    bus.publish(event("follow.state", "com.example.follow", 1));
    bus.publish(event("nav.state", "com.example.nav", 2));
    let got = next_json(&mut lines).await;
    assert_eq!(got["topic"], "nav.state");
    assert_eq!(got["publisher"], "com.example.nav");
    assert_eq!(got["payload"]["n"], 2);

    task.abort();
}

#[tokio::test]
async fn a_bad_request_is_answered_with_an_error_and_closed() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new());
    let (path, task) = serve_event_export(bus, dir.path().join("plugin-host")).unwrap();

    let mut lines = connect(&path, r#"{"v":1,"op":"subscribe","topics":[]}"#).await;
    assert!(next_json(&mut lines).await["error"].is_string());
    let end = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap();
    assert!(end.is_none(), "the host closes after an error");

    task.abort();
}

#[tokio::test]
async fn a_client_that_hangs_up_releases_its_bus_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new());
    let (path, task) = serve_event_export(bus.clone(), dir.path().join("plugin-host")).unwrap();

    let mut lines = connect(&path, r#"{"v":1,"op":"subscribe","topics":["a"]}"#).await;
    assert_eq!(next_json(&mut lines).await["ok"], true);
    assert_eq!(bus.receiver_count(), 1);
    drop(lines);
    for _ in 0..100 {
        if bus.receiver_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(bus.receiver_count(), 0);

    task.abort();
}

#[tokio::test]
async fn the_export_socket_sits_in_the_root_only_control_dir() {
    let dir = tempfile::tempdir().unwrap();
    let control_dir = dir.path().join("plugin-host");
    let (path, task) = serve_event_export(Arc::new(EventBus::new()), control_dir.clone()).unwrap();

    assert_eq!(path, export_socket_path(&control_dir));
    let dir_mode = std::fs::metadata(&control_dir)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700);
    let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(sock_mode & 0o777, 0o600);

    task.abort();
}
