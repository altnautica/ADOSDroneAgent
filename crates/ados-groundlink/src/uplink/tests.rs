use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::ops::{self, TakeContext, TakeRequest};
use super::watcher::{classify, GroundFrame};
use super::*;

const OK_CTX: TakeContext = TakeContext {
    bind_active: false,
    direct_role: true,
};

fn new_uplink(dir: &tempfile::TempDir) -> Arc<Uplink> {
    Arc::new(Uplink::with_sidecar(dir.path().join("wfb-stats.json")))
}

fn take_req(id: &str, key: &str) -> TakeRequest {
    TakeRequest {
        holder_device_id: id.into(),
        holder_name: "Ops phone".into(),
        phone_key: key.into(),
        yield_url: Some("http://192.168.1.40:47811/uplink/yield".into()),
    }
}

/// A receive generation as the daemon runs it: attaches, takes `switch` to
/// carry out each later ask, and reports it. `stopping` is raised from the
/// moment a stop begins until the processes are gone.
fn fake_generation(u: &Arc<Uplink>, switch: Duration, stopping: Arc<AtomicBool>) {
    let mut asks = u.attach_generation();
    let first = asks.first();
    u.report_applied(first.version, first.transmit);
    let u = u.clone();
    tokio::spawn(async move {
        while let Some(ask) = asks.next().await {
            if !ask.transmit {
                stopping.store(true, Ordering::SeqCst);
            }
            tokio::time::sleep(switch).await;
            u.report_applied(ask.version, ask.transmit);
            stopping.store(false, Ordering::SeqCst);
        }
    });
}

#[test]
fn a_box_starts_transmitting_and_says_so_on_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let u = Uplink::with_sidecar(dir.path().join("wfb-stats.json"));
    assert_eq!(u.state(), UplinkState::Transmitting);
    let v = u.sidecar_json();
    assert_eq!(v["state"], json!("transmitting"));
    assert_eq!(v["holder_device_id"], Value::Null);
    assert_eq!(v["holder_name"], Value::Null);
    assert!(v["since_ms"].as_u64().unwrap() > 0);
    assert_eq!(v["watcher"]["last_beacon_ms"], Value::Null);
    assert!(u.wants_transmit(), "a box never boots silent");
}

#[test]
fn the_holder_window_is_the_drones_peer_stale_threshold() {
    assert_eq!(HOLDER_STALE.as_secs_f64(), ados_radio::hop::PEER_STALE_SECS);
}

#[test]
fn a_beacon_carries_at_most_sixteen_bytes_of_the_id() {
    assert_eq!(beacon_id("a1b2c3d4e5f6"), "a1b2c3d4e5f6");
    assert_eq!(beacon_id("0123456789abcdefXYZ"), "0123456789abcdef");
}

#[tokio::test(start_paused = true)]
async fn a_take_replies_only_after_both_transmitters_exited() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    let stopping = Arc::new(AtomicBool::new(false));
    fake_generation(u, Duration::from_millis(1500), stopping.clone());
    tokio::task::yield_now().await;
    assert!(u.transmitters_up());

    let reply = ops::take(u, take_req("a1b2c3d4e5f6", "KEY-1"), OK_CTX).await;
    // The phone starts on this reply, so nothing may still be transmitting.
    assert!(!u.transmitters_up(), "the take replied while a transmitter ran");
    assert!(!stopping.load(Ordering::SeqCst));
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(reply["state"], json!("standby"));
    assert_eq!(reply["uplink"]["holder_device_id"], json!("a1b2c3d4e5f6"));
    assert_eq!(reply["uplink"]["holder_name"], json!("Ops phone"));

    // The transition is on the sidecar at once.
    let side: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("wfb-stats.json")).unwrap())
            .unwrap();
    assert_eq!(side["uplink"]["state"], json!("standby"));
}

/// The start race: a take that lands after the receive generation read
/// "transmit" but before it reported its transmitters up must not reply until
/// the generation stopped them again.
#[tokio::test(start_paused = true)]
async fn a_take_while_the_generation_starts_its_transmitters_waits_for_their_stop() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let mut asks = u.attach_generation();
    let first = asks.first();
    let (gen_uplink, gen_log) = (u.clone(), log.clone());
    tokio::spawn(async move {
        // Spawning both transmitters takes a moment.
        tokio::time::sleep(Duration::from_millis(50)).await;
        gen_log.lock().unwrap().push("started");
        gen_uplink.report_applied(first.version, first.transmit);
        while let Some(ask) = asks.next().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
            gen_log
                .lock()
                .unwrap()
                .push(if ask.transmit { "started" } else { "stopped" });
            gen_uplink.report_applied(ask.version, ask.transmit);
        }
    });
    let reply = ops::take(u, take_req("a1b2c3d4e5f6", "KEY-1"), OK_CTX).await;
    log.lock().unwrap().push("standby");
    assert_eq!(reply["state"], json!("standby"));
    assert_eq!(*log.lock().unwrap(), ["started", "stopped", "standby"]);
}

#[tokio::test(start_paused = true)]
async fn a_take_with_no_receive_generation_stands_by_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    let reply = ops::take(u, take_req("a1b2", "K"), OK_CTX).await;
    assert_eq!(reply["state"], json!("standby"));
    assert!(!u.wants_transmit());
}

#[tokio::test(start_paused = true)]
async fn a_take_whose_transmitters_never_stop_is_refused_and_the_box_keeps_transmitting() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    // A generation that reports its pair up and never answers again.
    let asks = u.attach_generation();
    u.report_applied(asks.first().version, true);
    let reply = ops::take(u, take_req("a1b2", "K"), OK_CTX).await;
    assert_eq!(reply["error"], json!("E_TX_STOP_FAILED"));
    assert_eq!(u.state(), UplinkState::Transmitting);
    assert!(u.wants_transmit());
}

#[tokio::test(start_paused = true)]
async fn the_refusals_each_say_why_in_one_sentence() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    let bind = TakeContext {
        bind_active: true,
        direct_role: true,
    };
    let mesh = TakeContext {
        bind_active: false,
        direct_role: false,
    };
    for (ctx, code) in [(bind, "E_BIND_ACTIVE"), (mesh, "E_NOT_DIRECT")] {
        let r = ops::take(u, take_req("a1b2", "K"), ctx).await;
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["error"], json!(code));
        let detail = r["detail"].as_str().unwrap();
        assert!(
            detail.ends_with('.') && detail.matches(". ").count() == 0,
            "{detail}"
        );
        assert_eq!(u.state(), UplinkState::Transmitting);
    }
    let bad = ops::take(u, take_req("not an id!", "K"), OK_CTX).await;
    assert_eq!(bad["error"], json!("E_BAD_HOLDER"));

    ops::take(u, take_req("a1b2", "K"), OK_CTX).await;
    let other = ops::take(u, take_req("ffff", "K2"), OK_CTX).await;
    assert_eq!(other["error"], json!("E_UPLINK_HELD"));
    assert_eq!(
        other["detail"],
        json!("The uplink is already held by Ops phone.")
    );
    // The holder retrying its own take is answered, not refused.
    let again = ops::take(u, take_req("a1b2", "K"), OK_CTX).await;
    assert_eq!(again["state"], json!("standby"));
}

#[tokio::test(start_paused = true)]
async fn only_the_holding_phone_gives_back_and_the_box_transmits_again() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    let stopping = Arc::new(AtomicBool::new(false));
    fake_generation(u, Duration::from_millis(200), stopping);
    tokio::task::yield_now().await;

    let none = ops::give_back(u, "a1b2", "K").await;
    assert_eq!(none["error"], json!("E_NOT_HELD"));

    ops::take(u, take_req("a1b2", "K"), OK_CTX).await;
    let wrong_id = ops::give_back(u, "ffff", "K").await;
    assert_eq!(wrong_id["error"], json!("E_NOT_HOLDER"));
    let wrong_key = ops::give_back(u, "a1b2", "OTHER").await;
    assert_eq!(wrong_key["error"], json!("E_NOT_HOLDER"));
    assert!(matches!(u.state(), UplinkState::Standby(_)));

    let back = ops::give_back(u, "a1b2", "K").await;
    assert_eq!(back["state"], json!("transmitting"));
    assert!(
        u.transmitters_up(),
        "the transmitters run again before the reply"
    );
    assert_eq!(back["uplink"]["holder_device_id"], Value::Null);
}

#[tokio::test(start_paused = true)]
async fn the_boxs_operator_takes_back_whether_or_not_the_phone_answered() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    // Idempotent while transmitting.
    let idle = ops::take_back(u, false).await;
    assert_eq!(idle["state"], json!("transmitting"));
    ops::take(u, take_req("a1b2", "K"), OK_CTX).await;
    let r = ops::take_back(u, false).await;
    assert_eq!(r["state"], json!("transmitting"));
    assert!(u.wants_transmit());
}

#[tokio::test(start_paused = true)]
async fn a_silent_holder_loses_the_uplink_after_the_stale_window_and_not_before() {
    let dir = tempfile::tempdir().unwrap();
    let u = &new_uplink(&dir);
    ops::take(u, take_req("a1b2c3d4e5f6", "K"), OK_CTX).await;
    let t0 = Instant::now();
    assert!(!ops::fall_back_if_absent(u, t0 + Duration::from_secs(24)).await);

    // A beacon from someone else does not keep the holder alive.
    assert!(!u.note_beacon("ffffffffffff", t0 + Duration::from_secs(20)));
    // The holder's does, and is stamped on the sidecar.
    assert!(u.note_beacon("a1b2c3d4e5f6", t0 + Duration::from_secs(20)));
    assert!(u.sidecar_json()["watcher"]["last_beacon_ms"].is_u64());
    assert!(!ops::fall_back_if_absent(u, t0 + Duration::from_secs(44)).await);
    assert!(matches!(u.state(), UplinkState::Standby(_)));

    assert!(ops::fall_back_if_absent(u, t0 + Duration::from_secs(45)).await);
    assert_eq!(u.state(), UplinkState::Transmitting);
    assert!(u.wants_transmit());
    // Nothing to fall back from any more.
    assert!(!ops::fall_back_if_absent(u, t0 + Duration::from_secs(90)).await);
}

#[test]
fn the_watcher_hears_grounds_and_hop_acks_and_nothing_unsigned() {
    let key = ados_radio::hop::derive_pair_key(None);
    let phone = ados_radio::hop::build_presence_beacon("a1b2c3d4e5f6", false, 149, 0, 1, &key);
    assert_eq!(
        classify(&phone, &key),
        GroundFrame::Beacon("a1b2c3d4e5f6".into())
    );
    let drone = ados_radio::hop::build_presence_beacon("drone-1", true, 149, 0, 1, &key);
    assert_eq!(classify(&drone, &key), GroundFrame::Unverified);
    let other_fleet = [7u8; 32];
    assert_eq!(classify(&phone, &other_fleet), GroundFrame::Unverified);
    let ack =
        ados_radio::hop::build_hop_announce(0, 157, ados_radio::hop::HopTrigger::Periodic, &key);
    assert_eq!(classify(&ack, &key), GroundFrame::HopAck);
    assert_eq!(classify(b"noise", &key), GroundFrame::Unverified);
}
