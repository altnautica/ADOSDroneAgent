//! Heartbeat field-name parity contract.
//!
//! The cloud relay schema (`proto/cloud/openapi.yaml::AgentHeartbeat`)
//! and the Python full agent emit metric keys in camelCase:
//!   * `cpuPercent`
//!   * `temperature`
//!   * `memoryUsedMb`
//!   * `memoryTotalMb`
//!
//! Earlier audits flagged a divergence where the lite agent had
//! emitted snake_case / mixed-case variants (`cpuPct`, `socTempC`,
//! `cpu_pct`, `soc_temp_c`). These keys would silently drop on the
//! cloud-relay decoder because the Python schema is camelCase. This
//! test pins the camelCase shape end-to-end by capturing the JSON
//! body the agent POSTs to `/agent/status` against a wiremock relay.
//!
//! It catches both directions of the bug:
//!   1. Banned legacy keys MUST NOT appear.
//!   2. Canonical camelCase keys MUST appear.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_cloud::{spawn_cloud_client, AgentMeta, CloudConfig, InboundChannels};
use ados_setup::diag::DiagState;
use ados_setup::pairing::{PairingState, PairingStore};
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Spin a wiremock relay that captures the raw POST body sent to
/// `/agent/status`. Returns the mock + a shared slot the test can
/// poll for the captured bytes. The slot is a `std::sync::Mutex`
/// because wiremock's `Respond` impl is synchronous and cannot
/// `.await` an async-mutex lock.
async fn capture_heartbeat_body() -> (MockServer, Arc<Mutex<Option<Vec<u8>>>>) {
    let mock = MockServer::start().await;
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let captured_for_mock = captured.clone();
    Mock::given(method("POST"))
        .and(path("/agent/status"))
        .respond_with(move |req: &Request| {
            // Stash the body on the first hit; subsequent hits do
            // not overwrite so the assertion sees the first
            // heartbeat the agent emits, not whichever one happens
            // to land just before the test's timeout.
            if let Ok(mut guard) = captured_for_mock.lock() {
                if guard.is_none() {
                    guard.replace(req.body.clone());
                }
            }
            ResponseTemplate::new(200)
        })
        .mount(&mock)
        .await;
    (mock, captured)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeat_uses_canonical_camelcase_keys() {
    let (mock, captured) = capture_heartbeat_body().await;

    // Pre-populate a paired pairing.json so the http_loop takes the
    // /agent/status path rather than /pairing/register.
    let tmp = tempfile::tempdir().unwrap();
    let pairing_path = tmp.path().join("pairing.json");
    let store = PairingStore::new(&pairing_path);
    let initial = PairingState {
        paired: true,
        api_key: Some("ados_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into()),
        owner_id: Some("user_test".into()),
        ..Default::default()
    };
    store.save(&initial).unwrap();

    let config = CloudConfig {
        device_id: "parity-device".into(),
        // Empty MQTT broker keeps the publish loop disabled.
        mqtt_broker: String::new(),
        mqtt_port: 1883,
        mqtt_use_tls: false,
        convex_url: mock.uri(),
        pairing_path,
        agent_meta: Some(AgentMeta {
            board_name: Some("Luckfox Pico Zero".into()),
            soc: Some("rv1106g3".into()),
            arch: Some("armv7".into()),
            ram_mb: Some(256),
            ..Default::default()
        }),
        mqtt_keepalive_secs: 60,
        connect_timeout_secs: 3,
        request_timeout_secs: 10,
        allow_reboot: false,
    };

    let (mavlink_tx, _mavlink_rx) = broadcast::channel::<Vec<u8>>(16);
    let diag = DiagState::shared();
    spawn_cloud_client(config, mavlink_tx, InboundChannels::default(), diag)
        .expect("spawn cloud client");

    // Wait for the http_loop to fire its first heartbeat. The base
    // tick interval is 5s when paired; the wiremock responds 200
    // immediately so we should see a body well within 7s.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let body_bytes = loop {
        if std::time::Instant::now() >= deadline {
            panic!(
                "no heartbeat captured within 8s; the http_loop did not POST \
                 to /agent/status against the mock relay"
            );
        }
        if let Ok(guard) = captured.lock() {
            if let Some(b) = guard.as_ref() {
                break b.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let value: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("heartbeat body decodes as JSON");
    let obj = value
        .as_object()
        .expect("heartbeat body is a JSON object");

    // Canonical camelCase keys MUST be present (see
    // proto/cloud/openapi.yaml::AgentHeartbeat). The Python full
    // agent and the lite agent both emit these identical names so
    // the cloud relay deserializer matches a single schema for both.
    for key in ["cpuPercent", "temperature", "memoryUsedMb", "memoryTotalMb"] {
        assert!(
            obj.contains_key(key),
            "heartbeat body must contain canonical camelCase key `{key}`; \
             keys present: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
    }

    // The two memory fields are integer megabytes per the schema.
    let mem_total = obj
        .get("memoryTotalMb")
        .expect("memoryTotalMb present")
        .as_u64();
    assert!(
        mem_total.unwrap_or(0) > 0,
        "memoryTotalMb must be a positive integer; got {:?}",
        mem_total
    );

    // The temperature field is nullable in the schema (boards
    // without thermal_zone0 emit null) but the KEY itself must be
    // present so the cloud-relay shape gate does not reject the
    // payload.
    assert!(
        obj.contains_key("temperature"),
        "temperature key must be present even when the value is null"
    );

    // Banned legacy keys MUST NOT appear. These were the gate-9
    // bugs flagged in the audit: an interim revision had emitted
    // these names and the cloud-relay decoder dropped them. Each
    // assertion pins one of the four observed regressions.
    for banned in ["cpuPct", "socTempC", "cpu_pct", "soc_temp_c"] {
        assert!(
            !obj.contains_key(banned),
            "heartbeat body must NOT contain legacy/snake_case key `{banned}`; \
             this name does not match proto/cloud/openapi.yaml. \
             Full key list: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
    }

    // Spot-check a couple of static fields to confirm we captured
    // a real heartbeat envelope and not a pairing beacon by mistake.
    assert_eq!(
        obj.get("deviceId").and_then(|v| v.as_str()),
        Some("parity-device"),
        "deviceId must echo the configured value"
    );
    assert_eq!(
        obj.get("runtimeMode").and_then(|v| v.as_str()),
        Some("lite"),
        "runtimeMode must be \"lite\" so the GCS pill renders correctly"
    );

    // Static board metadata flows from agent_meta and uses camelCase.
    assert_eq!(
        obj.get("boardName").and_then(|v| v.as_str()),
        Some("Luckfox Pico Zero"),
    );
    assert_eq!(
        obj.get("boardSoc").and_then(|v| v.as_str()),
        Some("rv1106g3"),
    );
}
