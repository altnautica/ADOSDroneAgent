use super::*;
use ados_protocol::pilot::{decode_request, Request};
use tokio::net::UnixListener;

/// A scripted pilot: answers `lease.acquire` with `lease_id` 7 (or `busy`),
/// publishes state, and records every request.
async fn fake_pilot(path: PathBuf, grant: bool) -> mpsc::Receiver<Request> {
    let listener = UnixListener::bind(&path).unwrap();
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut rd, wr) = stream.into_split();
        let wr = Arc::new(tokio::sync::Mutex::new(wr));
        let w2 = Arc::clone(&wr);
        tokio::spawn(async move {
            loop {
                let state =
                    pilot::encode_op("state", &serde_json::json!({"owner": "pilot"})).unwrap();
                if pilot::write_msg(&mut *w2.lock().await, &state)
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        while let Ok(Some(body)) = pilot::read_msg(&mut rd).await {
            let req = decode_request(&body).unwrap();
            if let Request::LeaseAcquire { .. } = &req {
                let reply = if grant {
                    serde_json::json!({"ok": true, "lease_id": 7})
                } else {
                    serde_json::json!({"ok": false, "err": "busy"})
                };
                let b = pilot::encode_op("lease.acquire", &reply).unwrap();
                pilot::write_msg(&mut *wr.lock().await, &b).await.unwrap();
            }
            if tx.send(req).await.is_err() {
                break;
            }
        }
    });
    rx
}

async fn ready(client: &PilotClient) {
    for _ in 0..100 {
        if client.reachable() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("pilot never reachable");
}

async fn next_non_sub(rx: &mut mpsc::Receiver<Request>) -> Request {
    loop {
        let r = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if r != Request::Subscribe {
            return r;
        }
    }
}

fn goal() -> Value {
    rmpv::ext::to_value(serde_json::json!({"kind": "position", "n": 1.0, "e": 2.0, "d": -3.0, "max_speed_mps": 4.0}))
        .unwrap()
}

#[tokio::test]
async fn acquire_goal_and_holder_only_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pilot.sock");
    let mut rx = fake_pilot(path.clone(), true).await;
    let bus = Arc::new(EventBus::new());
    let mut events = bus.subscribe();
    let client = PilotClient::spawn(path, Arc::clone(&bus));
    ready(&client).await;

    let reply = client
        .acquire("com.example.follow", "follow-me", "motion")
        .await;
    assert_eq!(lease_id_of(&reply), Some(7));
    match next_non_sub(&mut rx).await {
        Request::LeaseAcquire { holder, .. } => assert_eq!(holder.plugin_id, "com.example.follow"),
        other => panic!("{other:?}"),
    }
    // Only the holder may send goals.
    let refused = client.goal("com.example.other", 7, goal());
    assert_eq!(
        get(&refused, "err").and_then(Value::as_str),
        Some("not_holder")
    );
    client.goal("com.example.follow", 7, goal());
    assert!(matches!(
        next_non_sub(&mut rx).await,
        Request::Goal { lease_id: 7, .. }
    ));

    // A revocation is stamped with the holder and dropped from the table.
    client.on_message(
        "lease.revoked",
        rmpv::ext::to_value(serde_json::json!({"lease_id": 7, "reason": "pause"})).unwrap(),
    );
    let ev = loop {
        let e = events.recv().await.unwrap();
        if e.topic == REVOKED_TOPIC {
            break e;
        }
    };
    assert!(flight_event_for("com.example.follow", &ev.payload));
    assert!(!flight_event_for("com.example.other", &ev.payload));
    assert!(!client.holds("com.example.follow", 7));
}

#[tokio::test]
async fn stopping_a_plugin_releases_its_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pilot.sock");
    let mut rx = fake_pilot(path.clone(), true).await;
    let client = PilotClient::spawn(path, Arc::new(EventBus::new()));
    ready(&client).await;
    client.acquire("p", "s", "motion").await;
    next_non_sub(&mut rx).await;
    client.release_plugin("p");
    assert_eq!(
        next_non_sub(&mut rx).await,
        Request::LeaseRelease { lease_id: 7 }
    );
}

#[tokio::test]
async fn legacy_setpoints_take_and_return_an_implicit_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pilot.sock");
    let mut rx = fake_pilot(path.clone(), true).await;
    let client = PilotClient::spawn(path, Arc::new(EventBus::new()));
    ready(&client).await;
    assert!(client.legacy_goal("old.plugin", goal()).await);
    match next_non_sub(&mut rx).await {
        Request::LeaseAcquire { holder, .. } => assert_eq!(holder.skill_id, LEGACY_SKILL),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        next_non_sub(&mut rx).await,
        Request::Goal { lease_id: 7, .. }
    ));
    // Silence for over a second gives the lease back.
    assert_eq!(
        next_non_sub(&mut rx).await,
        Request::LeaseRelease { lease_id: 7 }
    );
}

#[tokio::test]
async fn busy_pilot_and_absent_pilot_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pilot.sock");
    let _rx = fake_pilot(path.clone(), false).await;
    let client = PilotClient::spawn(path, Arc::new(EventBus::new()));
    ready(&client).await;
    let r = client.acquire("p", "s", "motion").await;
    assert_eq!(get(&r, "err").and_then(Value::as_str), Some("busy"));

    let absent = PilotClient::spawn(dir.path().join("none.sock"), Arc::new(EventBus::new()));
    let r = absent.acquire("p", "s", "motion").await;
    assert_eq!(
        get(&r, "err").and_then(Value::as_str),
        Some("pilot_unavailable")
    );
}

#[test]
fn lease_id_accepts_integer_or_string() {
    let int = rmpv::ext::to_value(serde_json::json!({"lease_id": 12})).unwrap();
    let text = rmpv::ext::to_value(serde_json::json!({"lease_id": "12"})).unwrap();
    assert_eq!(lease_id_of(&int), Some(12));
    assert_eq!(lease_id_of(&text), Some(12));
}

#[tokio::test]
async fn goal_status_reaches_only_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pilot.sock");
    let _rx = fake_pilot(path.clone(), true).await;
    let bus = Arc::new(EventBus::new());
    let mut events = bus.subscribe();
    let client = PilotClient::spawn(path, Arc::clone(&bus));
    ready(&client).await;
    let reply = client
        .acquire("com.example.follow", "follow-me", "motion")
        .await;
    assert_eq!(lease_id_of(&reply), Some(7));

    // A status for a lease nobody holds goes nowhere.
    client.on_message(
        "goal.status",
        rmpv::ext::to_value(
            serde_json::json!({"lease_id": 8, "state": "enroute", "coverage": 0.5}),
        )
        .unwrap(),
    );
    client.on_message(
        "goal.status",
        rmpv::ext::to_value(serde_json::json!({
            "op": "goal.status", "lease_id": 7, "state": "arrived", "vantage_ne": [1.0, 2.0],
            "coverage": 0.9, "occluder_ne": [3.0, 0.0], "occluder_width_m": 0.8
        }))
        .unwrap(),
    );
    let ev = loop {
        let e = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        if e.topic == GOAL_STATUS_TOPIC {
            break e;
        }
    };
    assert!(flight_event_for("com.example.follow", &ev.payload));
    assert!(!flight_event_for("com.example.other", &ev.payload));
    let p: serde_json::Value = rmpv::ext::from_value(ev.payload.clone()).unwrap();
    assert_eq!(p["lease_id"], 7);
    assert_eq!(p["state"], "arrived");
    assert_eq!(p["vantage_ne"], serde_json::json!([1.0, 2.0]));
    assert_eq!(p["occluder_width_m"], 0.8);
    assert!(p.get("op").is_none());
}

#[test]
fn goal_status_is_a_holder_topic() {
    assert!(crate::handlers::FLIGHT_TOPICS_FOR_SUBSCRIBE.contains(&GOAL_STATUS_TOPIC));
}
