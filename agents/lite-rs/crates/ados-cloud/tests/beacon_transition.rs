//! Unpaired → paired transition contract.
//!
//! When the agent has no `pairing.json` (or the stored state is
//! `paired=false`), the cloud client emits a beacon to
//! `{convex_url}/pairing/register` every 30 s. The moment a valid
//! pairing.json appears (the operator typed the code into the GCS or
//! the wizard, which atomically writes pairing.json with `paired=true`
//! and a real api_key), the next tick of the http_loop must take the
//! heartbeat branch and emit to `/agent/status` instead — and stop
//! sending beacons.
//!
//! The unpaired tick interval is 30 s, so this test takes ~35 s of
//! real wall-clock time to span the transition in a single run. We
//! also ship two faster, decoupled tests that pin each side of the
//! transition independently so a CI run that gets killed at 30 s
//! still surfaces both halves of the contract.
//!
//! `tokio::time::pause()` would let us advance virtual time, but it
//! requires the `test-util` feature on the `tokio` dev-dep which is
//! a wider workspace change than this test bundle should make. The
//! real-time variant is good enough for a contract test that runs
//! once per CI invocation.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_cloud::{spawn_cloud_client, CloudConfig, InboundChannels};
use ados_setup::diag::DiagState;
use ados_setup::pairing::{PairingState, PairingStore};
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Wiremock mounts that count hits on the two endpoints. The mocks
/// answer 200 immediately; the test asserts on the counters.
async fn mount_counting_endpoints(mock: &MockServer) -> (Arc<AtomicU32>, Arc<AtomicU32>) {
    let beacon_count = Arc::new(AtomicU32::new(0));
    let heartbeat_count = Arc::new(AtomicU32::new(0));

    let beacon_for_mock = beacon_count.clone();
    Mock::given(method("POST"))
        .and(path("/pairing/register"))
        .respond_with(move |_: &Request| {
            beacon_for_mock.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
        })
        .mount(mock)
        .await;

    let heartbeat_for_mock = heartbeat_count.clone();
    Mock::given(method("POST"))
        .and(path("/agent/status"))
        .respond_with(move |_: &Request| {
            heartbeat_for_mock.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
        })
        .mount(mock)
        .await;

    (beacon_count, heartbeat_count)
}

fn make_config(mock_uri: String, pairing_path: std::path::PathBuf) -> CloudConfig {
    CloudConfig {
        device_id: "transition-device".into(),
        // MQTT disabled — the transition is purely an HTTP-loop concern.
        mqtt_broker: String::new(),
        mqtt_port: 1883,
        mqtt_use_tls: false,
        convex_url: mock_uri,
        pairing_path,
        agent_meta: None,
        mqtt_keepalive_secs: 60,
        connect_timeout_secs: 3,
        request_timeout_secs: 10,
        allow_reboot: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpaired_emits_beacon_and_no_heartbeat() {
    // Fast half-A test: with no pairing.json, the http_loop's first
    // tick MUST hit /pairing/register (beacon) and MUST NOT hit
    // /agent/status (heartbeat). Captures the unpaired side of the
    // transition without waiting 30 s.
    let mock = MockServer::start().await;
    let (beacon_count, heartbeat_count) = mount_counting_endpoints(&mock).await;

    let tmp = tempfile::tempdir().unwrap();
    // No pairing.json on disk — the PairingStore::load() returns the
    // default unpaired state.
    let pairing_path = tmp.path().join("pairing.json");
    let config = make_config(mock.uri(), pairing_path);

    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx, InboundChannels::default(), diag)
        .expect("spawn cloud client");

    // First tick fires immediately on loop entry. Allow up to 5 s
    // for the wiremock round-trip plus jitter.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if beacon_count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        beacon_count.load(Ordering::SeqCst) >= 1,
        "unpaired loop must POST at least one beacon to /pairing/register \
         within 5 s; got {}",
        beacon_count.load(Ordering::SeqCst)
    );
    assert_eq!(
        heartbeat_count.load(Ordering::SeqCst),
        0,
        "unpaired loop must NOT POST to /agent/status; got {} hits",
        heartbeat_count.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paired_emits_heartbeat_and_no_beacon() {
    // Fast half-B test: with a valid pairing.json, the first tick
    // MUST hit /agent/status and MUST NOT hit /pairing/register.
    let mock = MockServer::start().await;
    let (beacon_count, heartbeat_count) = mount_counting_endpoints(&mock).await;

    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");
    let store = PairingStore::new(&pairing_path);
    let initial = PairingState {
        paired: true,
        api_key: Some("ados_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into()),
        owner_id: Some("user_test".into()),
        ..Default::default()
    };
    store.save(&initial).expect("seed pairing.json");

    let config = make_config(mock.uri(), pairing_path);
    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx, InboundChannels::default(), diag)
        .expect("spawn cloud client");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if heartbeat_count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        heartbeat_count.load(Ordering::SeqCst) >= 1,
        "paired loop must POST at least one heartbeat to /agent/status \
         within 5 s; got {}",
        heartbeat_count.load(Ordering::SeqCst)
    );
    assert_eq!(
        beacon_count.load(Ordering::SeqCst),
        0,
        "paired loop must NOT POST to /pairing/register; got {} hits",
        beacon_count.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn beacon_to_heartbeat_transition_is_smooth() {
    // Slow end-to-end transition: start unpaired, observe a beacon,
    // write pairing.json, wait for the next tick (~30 s of unpaired
    // sleep + a small heartbeat-tick budget), observe a heartbeat,
    // verify the beacon counter has stabilized so the agent stopped
    // beaconing once it transitioned to paired.
    //
    // Total wall-clock budget: ~40 s. A test in this band runs once
    // per CI invocation and pays for itself by pinning the live
    // re-read of pairing state every iteration.
    let mock = MockServer::start().await;
    let (beacon_count, heartbeat_count) = mount_counting_endpoints(&mock).await;

    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");
    let store = PairingStore::new(&pairing_path);

    let config = make_config(mock.uri(), pairing_path);
    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx, InboundChannels::default(), diag)
        .expect("spawn cloud client");

    // Step A: capture the first beacon.
    let step_a_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < step_a_deadline {
        if beacon_count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        beacon_count.load(Ordering::SeqCst) >= 1,
        "step A (unpaired): expected at least one beacon within 5 s"
    );
    assert_eq!(
        heartbeat_count.load(Ordering::SeqCst),
        0,
        "step A (unpaired): no heartbeat should have fired yet"
    );

    let beacons_before_pair = beacon_count.load(Ordering::SeqCst);

    // Step B: simulate the operator pairing through the wizard. The
    // PairingStore writes the file atomically with paired=true and a
    // valid api_key. The next tick of the http_loop reads the new
    // state and switches to the heartbeat branch.
    let paired_state = PairingState {
        paired: true,
        api_key: Some("ados_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into()),
        owner_id: Some("user_transition".into()),
        ..Default::default()
    };
    store.save(&paired_state).expect("paired state persists");

    // Step C: wait for the next tick. The unpaired tick sleeps 30 s
    // after a successful beacon; we budget 35 s plus a small slack.
    let step_c_deadline = std::time::Instant::now() + Duration::from_secs(35);
    while std::time::Instant::now() < step_c_deadline {
        if heartbeat_count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        heartbeat_count.load(Ordering::SeqCst) >= 1,
        "step C (post-pair): expected at least one heartbeat within 35 s of \
         pairing.json being written; got {} heartbeats and {} beacons",
        heartbeat_count.load(Ordering::SeqCst),
        beacon_count.load(Ordering::SeqCst)
    );

    // Step D: confirm the agent stopped beaconing once paired. We
    // allow at most one additional beacon between the pairing write
    // and the loop's first paired-state read (a tick that started
    // before the file write would still call send_pairing_beacon).
    let beacons_after = beacon_count.load(Ordering::SeqCst);
    assert!(
        beacons_after <= beacons_before_pair + 1,
        "step D: agent must stop beaconing after pairing.json is written; \
         observed {} beacons before pair and {} beacons after \
         (allowance: at most +1 for the in-flight tick)",
        beacons_before_pair,
        beacons_after
    );
}
