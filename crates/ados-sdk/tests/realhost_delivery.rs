//! The SDK's relay-backed and state-backed host methods against the real host.
//!
//! Drives the SDK facades through a live `ados-plugin-host` server backed by
//! [`RealHost`], whose cloud-publish socket and vehicle-state socket are local
//! stubs and whose offload-link sidecar lives in a temp dir. What the stubs
//! decode or serve and what the sidecar holds is what the real relay, state hub
//! and perception-tier reader see, so this proves the SDK's argument names and
//! encodings are the ones the host handlers read, and that the plugin id on the
//! wire is the caller's verified identity.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ados_plugin_host::realhost::{NodeInfoSources, RealHost};
use ados_plugin_host::{EventBus, PluginIpcServer};
use ados_protocol::cloud_publish::{CloudPublishKind, CloudPublishReply, CloudPublishRequest};
use ados_protocol::plugin::TokenIssuer;
use ados_sdk::{ClientError, OffloadAdvertisement, PluginContext, PluginIpcClient};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PLUGIN_ID: &str = "com.example.mapper";

/// Accept `count` relay exchanges on `path`, answer each with `reply`, and
/// return the decoded requests.
fn relay_stub(
    path: std::path::PathBuf,
    count: usize,
    reply: CloudPublishReply,
) -> tokio::task::JoinHandle<Vec<CloudPublishRequest>> {
    let listener = tokio::net::UnixListener::bind(&path).expect("bind relay stub");
    tokio::spawn(async move {
        let mut seen = Vec::new();
        for _ in 0..count {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut len = [0u8; 4];
            stream.read_exact(&mut len).await.expect("read len");
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            stream.read_exact(&mut body).await.expect("read body");
            seen.push(CloudPublishRequest::decode(&body).expect("decode"));
            stream
                .write_all(&reply.encode().expect("encode reply"))
                .await
                .expect("write reply");
        }
        seen
    })
}

struct Harness {
    ctx: PluginContext,
    ipc: Arc<PluginIpcClient>,
    relay: std::path::PathBuf,
    vehicle_state: std::path::PathBuf,
    offload_link: std::path::PathBuf,
    _accept: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn harness(granted: &[&str]) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let relay = dir.path().join("cloud-publish.sock");
    let offload_link = dir.path().join("offload-link.json");
    let vehicle_state = dir.path().join("state.sock");
    let issuer = Arc::new(TokenIssuer::new(b"realhost-delivery-secret".to_vec()));
    let host = Arc::new(
        RealHost::new()
            .with_cloud_publish_path(relay.clone())
            .with_offload_link_path(offload_link.clone())
            .with_vehicle_state_socket(vehicle_state.clone())
            .with_node_info_sources(NodeInfoSources {
                config_yaml: dir.path().join("config.yaml"),
                profile_conf: dir.path().join("profile.conf"),
                mesh_role: dir.path().join("mesh-role"),
                board_sidecar: dir.path().join("board.json"),
                camera_state: dir.path().join("camera-state.json"),
            }),
    );
    let server = PluginIpcServer::new(dir.path(), issuer.clone(), Arc::new(EventBus::new()), host);
    let (path, accept) = server.serve_plugin(PLUGIN_ID).expect("bind plugin socket");
    let caps: BTreeSet<String> = granted.iter().map(|s| s.to_string()).collect();
    let token = issuer.mint(PLUGIN_ID, &caps, 600).to_token_string();
    let ipc = Arc::new(PluginIpcClient::new(PLUGIN_ID, token, &path));
    ipc.connect().await.expect("connect + handshake");
    let ctx = PluginContext::new(ipc.clone(), "1.0.0", "agent-1", None, BTreeMap::new());
    Harness {
        ctx,
        ipc,
        relay,
        vehicle_state,
        offload_link,
        _accept: accept,
        dir: dir.path().to_path_buf(),
        _dir: dir,
    }
}

fn ok_flag(reply: &Value) -> Option<bool> {
    reply
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some("ok"))
        .and_then(|(_, v)| v.as_bool())
}

#[tokio::test]
async fn publish_and_record_reach_the_relay_under_the_plugin_id() {
    let h = harness(&["cloud.publish", "cloud.records"]).await;
    let relay = relay_stub(h.relay.clone(), 2, CloudPublishReply::accepted());

    let published = h
        .ctx
        .cloud
        .publish("track.pose", &[1, 2, 3])
        .await
        .expect("publish");
    assert_eq!(ok_flag(&published), Some(true));

    let data = Value::Map(vec![
        (Value::from("state"), Value::from("done")),
        (Value::from("frames"), Value::from(42)),
    ]);
    let stored = h
        .ctx
        .cloud
        .put_record("jobs", "job-1", data, Some("drone01"))
        .await
        .expect("put record");
    assert_eq!(ok_flag(&stored), Some(true));

    let seen = relay.await.unwrap();
    assert_eq!(seen[0].kind, CloudPublishKind::Stream);
    assert_eq!(seen[0].plugin_id, PLUGIN_ID);
    assert_eq!(seen[0].stream.as_deref(), Some("track.pose"));
    assert_eq!(seen[0].payload, vec![1, 2, 3]);

    assert_eq!(seen[1].kind, CloudPublishKind::Record);
    assert_eq!(seen[1].plugin_id, PLUGIN_ID);
    assert_eq!(seen[1].collection.as_deref(), Some("jobs"));
    assert_eq!(seen[1].key.as_deref(), Some("job-1"));
    assert_eq!(seen[1].device_id.as_deref(), Some("drone01"));
    let json: serde_json::Value = serde_json::from_slice(&seen[1].payload).unwrap();
    assert_eq!(json, serde_json::json!({"state": "done", "frames": 42}));

    h.ipc.close().await;
}

#[tokio::test]
async fn each_call_needs_its_own_capability() {
    let h = harness(&["cloud.publish"]).await;
    let err = h
        .ctx
        .cloud
        .put_record("jobs", "job-1", Value::Map(vec![]), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::CapabilityDenied(cap) if cap.contains("cloud.records")),
        "{err:?}"
    );
    h.ipc.close().await;
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn an_advertised_offload_link_is_what_the_tier_reader_sees() {
    let h = harness(&["vision.detection.publish"]).await;
    let advert = OffloadAdvertisement {
        paired: true,
        bearer_acceptable: true,
        target: Some("10.0.0.9:8092".to_string()),
        device_id: None,
        model_id: Some("yolo-n".to_string()),
    };
    let reply = h
        .ctx
        .vision
        .advertise_offload(&advert)
        .await
        .expect("advertise");
    assert_eq!(ok_flag(&reply), Some(true));

    let link = ados_protocol::offload_link::read_offload_link_from(&h.offload_link, now_ms())
        .expect("a fresh link is live");
    assert!(link.paired && link.bearer_acceptable);
    assert_eq!(link.target.as_deref(), Some("10.0.0.9:8092"));
    assert_eq!(link.device_id, None);
    assert_eq!(link.model_id.as_deref(), Some("yolo-n"));

    // Dropping the link is an advertisement too, and it replaces the last one.
    h.ctx
        .vision
        .advertise_offload(&OffloadAdvertisement::default())
        .await
        .expect("advertise unpaired");
    let link = ados_protocol::offload_link::read_offload_link_from(&h.offload_link, now_ms())
        .expect("still fresh");
    assert!(!link.paired);
    assert_eq!(link.target, None);

    h.ipc.close().await;
}

#[tokio::test]
async fn node_info_arrives_typed_and_needs_its_capability() {
    let h = harness(&["node.info.read"]).await;
    std::fs::write(
        h.dir.join("config.yaml"),
        "agent:\n  profile: drone\nvideo:\n  camera: { width: 1280, height: 720, fps: 30 }\n",
    )
    .unwrap();
    let board = serde_json::json!({
        "version": 1, "name": "Raspberry Pi 4B", "model": "Raspberry Pi 4 Model B Rev 1.4",
        "tier": 3, "ram_mb": 4096, "cpu_cores": 4, "vendor": "Raspberry Pi", "soc": "BCM2711",
        "arch": "aarch64", "hw_video_codecs": [], "npu_tops": 0.0, "has_accelerator": false,
        "local_inference": "none", "has_local_inference": false,
    });
    std::fs::write(h.dir.join("board.json"), board.to_string()).unwrap();

    let info = h.ctx.node.info().await.expect("node.info");
    assert_eq!(info.profile, "drone");
    let board = info.board.expect("a published board");
    assert_eq!(board.id, "Raspberry Pi 4B");
    assert!(!board.has_npu);
    assert_eq!(info.ground_station.role, None);
    // No camera-state sidecar: not ready, but the configured stream is known.
    assert!(!info.camera.ready);
    let main = info.camera.main.expect("configured main stream");
    assert_eq!((main.width, main.height, main.fps), (1280, 720, 30));
    h.ipc.close().await;

    let denied = harness(&[]).await;
    let err = denied.ctx.node.info().await.unwrap_err();
    assert!(
        matches!(&err, ClientError::CapabilityDenied(cap) if cap.contains("node.info.read")),
        "{err:?}"
    );
    denied.ipc.close().await;
}

/// The SDK's mDNS requests arrive whole: the host reads the service type, the
/// port and the browse window the SDK encoded (each refusal below names the
/// value it read), and each method needs its own network capability. Nothing
/// here reaches the network: every call is refused before a record or a
/// browse would start.
#[tokio::test]
async fn mdns_requests_reach_the_host_whole_and_need_their_capabilities() {
    let h = harness(&["network.listen", "network.outbound"]).await;
    let txt = BTreeMap::from([("deviceId".to_string(), "compute-1".to_string())]);

    // No declared listen ports for this plugin on this node.
    let err = h
        .ctx
        .mdns
        .advertise("_ados-compute._tcp", 8092, &txt)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::Rpc(m) if m.contains("port 8092 is not a listen port")),
        "{err:?}"
    );
    let err = h
        .ctx
        .mdns
        .advertise("_ados._tcp", 8092, &txt)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::Rpc(m) if m.contains("_ados._tcp is published by the agent")),
        "{err:?}"
    );
    let err = h
        .ctx
        .mdns
        .browse("not-a-type", std::time::Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::Rpc(m) if m.contains("not-a-type")),
        "{err:?}"
    );
    h.ipc.close().await;

    let denied = harness(&[]).await;
    let err = denied
        .ctx
        .mdns
        .advertise("_ados-compute._tcp", 8092, &txt)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::CapabilityDenied(cap) if cap.contains("network.listen")),
        "{err:?}"
    );
    let err = denied
        .ctx
        .mdns
        .browse("_ados-compute._tcp", std::time::Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ClientError::CapabilityDenied(cap) if cap.contains("network.outbound")),
        "{err:?}"
    );
    denied.ipc.close().await;
}

/// Serve one connection on `path` the way the MAVLink service's state hub
/// does: write the given frames, then hold the stream open.
fn state_hub(path: std::path::PathBuf, frames: Vec<Vec<u8>>) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(&path).expect("bind state stub");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        for frame in frames {
            stream.write_all(&frame).await.expect("write state frame");
        }
        std::future::pending::<()>().await;
    })
}

fn field<'a>(map: &'a Value, key: &str) -> Option<&'a Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// Contract B has two wires and every consumer reads both; the snapshot a
/// plugin receives is the state object whichever one carried it.
#[tokio::test]
async fn telemetry_subscribe_pushes_the_vehicle_state_from_either_wire() {
    let h = harness(&["telemetry.read"]).await;
    let v1 = ados_protocol::state::encode_v1(&serde_json::json!({
        "armed": false, "mode": "STABILIZE", "seq": 1
    }))
    .unwrap();
    let v2 = ados_protocol::state::encode_v2(&serde_json::json!({
        "armed": true, "mode": "GUIDED", "seq": 2, "position": {"lat": 12.97, "lon": 77.59}
    }))
    .unwrap();
    let _hub = state_hub(h.vehicle_state.clone(), vec![v1, v2]);

    let (tx, mut states) = tokio::sync::mpsc::unbounded_channel();
    h.ctx
        .telemetry
        .subscribe(Arc::new(move |state| {
            let _ = tx.send(state);
        }))
        .await
        .expect("subscribe");

    let first = next_state(&mut states).await;
    assert_eq!(field(&first, "seq").and_then(Value::as_i64), Some(1));
    assert_eq!(
        field(&first, "mode").and_then(Value::as_str),
        Some("STABILIZE")
    );
    assert_eq!(field(&first, "armed").and_then(Value::as_bool), Some(false));

    let second = next_state(&mut states).await;
    assert_eq!(field(&second, "seq").and_then(Value::as_i64), Some(2));
    assert_eq!(
        field(&second, "mode").and_then(Value::as_str),
        Some("GUIDED")
    );
    assert_eq!(field(&second, "armed").and_then(Value::as_bool), Some(true));
    let lat = field(&second, "position")
        .and_then(|p| field(p, "lat"))
        .and_then(Value::as_f64);
    assert_eq!(lat, Some(12.97));

    h.ipc.close().await;
}

async fn next_state(states: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), states.recv())
        .await
        .expect("a snapshot arrived")
        .expect("stream open")
}
