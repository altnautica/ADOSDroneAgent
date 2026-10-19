use std::sync::Arc;

use tokio::io::BufReader;
use tokio::sync::Mutex as AsyncMutex;

use super::*;

/// One request a fake upstream saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

type SeenLog = Arc<AsyncMutex<Vec<Seen>>>;

/// Read one HTTP request off `stream`, log it, and answer `reply` verbatim.
async fn serve_one<S: AsyncRead + AsyncWrite + Unpin>(stream: S, reply: &[u8], seen: &SeenLog) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap().to_string();
    let path = parts.next().unwrap().to_string();
    let (mut length, mut content_type) = (0usize, None);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).await.unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (k, v) = header.split_once(':').unwrap();
        match k.to_ascii_lowercase().as_str() {
            "content-length" => length = v.trim().parse().unwrap(),
            "content-type" => content_type = Some(v.trim().to_string()),
            _ => {}
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await.unwrap();
    seen.lock().await.push(Seen {
        method,
        path,
        content_type,
        body,
    });
    let mut stream = reader.into_inner();
    stream.write_all(reply).await.unwrap();
    stream.shutdown().await.unwrap();
}

/// A fake media server on a loopback port answering each connection with the
/// next canned reply.
async fn fake_media_server(replies: Vec<&'static [u8]>) -> (u16, SeenLog) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: SeenLog = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        for reply in replies {
            let (stream, _) = listener.accept().await.unwrap();
            serve_one(stream, reply, &log).await;
        }
    });
    (port, seen)
}

/// A port nothing listens on.
async fn dead_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

fn args(entries: &[(&str, &str)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(k, v)| (Value::from(*k), Value::from(*v)))
            .collect(),
    )
}

fn offer_reply(v: HostResult) -> WhepOfferReply {
    rmpv::ext::from_value(v).unwrap()
}

const ANSWER: &[u8] = b"HTTP/1.1 201 Created\r\nContent-Type: application/sdp\r\n\
Location: /main/whep/5e1f-aa_9\r\nTransfer-Encoding: chunked\r\n\r\n\
5\r\nv=0\r\n\r\n6\r\na=x:1\n\r\n0\r\n\r\n";
const DELETED: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";

/// A drone roster: a primary camera with its own id, an enabled secondary and
/// a disabled one.
const ROSTER: &str = "\
agent:
  profile: drone
cameras:
  - { id: eo, source: /dev/video2, role: primary }
  - { id: cam2, source: rtsp://pod/ir }
  - { id: belly, source: /dev/video4, enabled: false }
";

/// A host whose agent config is `config` (written into `dir`), relaying WHEP
/// to `port`.
fn host_with_config(dir: &Path, config: &str, port: u16) -> RealHost {
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    RealHost::new()
        .with_media_relay(port, PathBuf::from("/nonexistent/control.sock"))
        .with_node_info_sources(NodeInfoSources {
            config_yaml: dir.join("config.yaml"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
            board_sidecar: dir.join("board.json"),
            camera_state: dir.join("camera-state.json"),
        })
}

fn is_unknown_camera(err: &HostError) -> bool {
    matches!(err, HostError::Rpc(m) if m.starts_with("unknown_camera:"))
}

#[tokio::test]
async fn an_offer_is_relayed_and_its_location_is_the_plugins_alone() {
    let (port, seen) = fake_media_server(vec![ANSWER, DELETED]).await;
    let dir = tempfile::tempdir().unwrap();
    let host = host_with_config(dir.path(), ROSTER, port);

    // The primary camera `eo` is served from the `main` leg.
    let out = host
        .relay_whep_offer("com.a", &args(&[("camera_id", "eo"), ("sdp", "OFFER")]))
        .await
        .unwrap();
    let reply = offer_reply(out);
    assert_eq!(reply.sdp, "v=0\r\na=x:1\n");
    let location = reply.location.expect("location");
    assert_eq!(location, "/whep/5e1f-aa_9?camera=main");
    {
        let seen = seen.lock().await;
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, "/main/whep");
        assert_eq!(seen[0].content_type.as_deref(), Some("application/sdp"));
        assert_eq!(seen[0].body, b"OFFER");
    }

    // Another plugin cannot close it; nothing reaches the media server.
    let err = host
        .relay_whep_close("com.b", &args(&[("location", &location)]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, HostError::Rpc(m) if m.starts_with("whep_unavailable:")),
        "{err:?}"
    );
    assert_eq!(seen.lock().await.len(), 1);

    // The owner closes it; the session is then forgotten.
    let closed = host
        .relay_whep_close("com.a", &args(&[("location", &location)]))
        .await
        .unwrap();
    assert_eq!(
        closed,
        Value::Map(vec![(Value::from("closed"), Value::Boolean(true))])
    );
    {
        let seen = seen.lock().await;
        assert_eq!(seen[1].method, "DELETE");
        assert_eq!(seen[1].path, "/main/whep/5e1f-aa_9");
    }
    assert!(host
        .relay_whep_close("com.a", &args(&[("location", &location)]))
        .await
        .is_err());
}

#[tokio::test]
async fn a_forged_location_is_refused_before_anything_is_sent() {
    let (port, seen) = fake_media_server(vec![]).await;
    let host = RealHost::new().with_media_relay(port, PathBuf::from("/nonexistent"));
    for loc in [
        "/whep/x?camera=main",
        "/main/whep/x",
        "http://127.0.0.1:9/x",
    ] {
        assert!(host
            .relay_whep_close("com.a", &args(&[("location", loc)]))
            .await
            .is_err());
    }
    assert!(seen.lock().await.is_empty());
}

#[tokio::test]
async fn only_a_configured_enabled_camera_reaches_the_media_server() {
    let (port, seen) = fake_media_server(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let host = host_with_config(dir.path(), ROSTER, port);
    // `main` is a media path, not a camera; `belly` is disabled; the rest are
    // not cameras of this node at all.
    for camera in ["", "../x", "main/whep", "a b", "main", "belly", "missing"] {
        let err = host
            .relay_whep_offer("p", &args(&[("camera_id", camera), ("sdp", "o")]))
            .await
            .unwrap_err();
        assert!(is_unknown_camera(&err), "{camera}: {err:?}");
    }
    assert!(seen.lock().await.is_empty());

    // A ground station carries no onboard camera, whatever its config lists.
    let gs = tempfile::tempdir().unwrap();
    let host = host_with_config(
        gs.path(),
        &ROSTER.replace("profile: drone", "profile: ground_station"),
        port,
    );
    let err = host
        .relay_whep_offer("p", &args(&[("camera_id", "eo"), ("sdp", "o")]))
        .await
        .unwrap_err();
    assert!(is_unknown_camera(&err), "{err:?}");
    assert!(seen.lock().await.is_empty());
}

#[tokio::test]
async fn bad_offers_and_a_down_media_server_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_with_config(dir.path(), ROSTER, dead_port().await);
    let err = host
        .relay_whep_offer("p", &args(&[("camera_id", "eo"), ("sdp", "")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, HostError::Rpc(m) if m.contains("arguments are invalid")),
        "{err:?}"
    );
    let err = host
        .relay_whep_offer("p", &args(&[("camera_id", "eo"), ("sdp", "o")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, HostError::Rpc(m) if m.starts_with("whep_unavailable: media server unreachable")),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_refused_offer_is_whep_unavailable() {
    let (port, _) =
        fake_media_server(vec![b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]).await;
    let dir = tempfile::tempdir().unwrap();
    let host = host_with_config(dir.path(), ROSTER, port);
    let err = host
        .relay_whep_offer("p", &args(&[("camera_id", "cam2"), ("sdp", "o")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, HostError::Rpc(m) if m == "whep_unavailable: media server answered 404 for camera cam2"),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_rig_goes_to_the_control_planes_route_and_its_answer_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("control.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let seen: SeenLog = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let reply = b"HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\n\
content-length: 43\r\n\r\n{\"error\":\"E_RIG_INVALID\",\"detail\":\"no rig\"}";
        serve_one(stream, reply, &log).await;
    });
    let host = RealHost::new().with_media_relay(dead_port().await, sock);
    let out = host
        .relay_rig_write(&args(&[("rig_json", "{\"v\":1}")]))
        .await
        .unwrap();
    let reply: RigWriteReply = rmpv::ext::from_value(out).unwrap();
    assert_eq!(reply.status, 400);
    assert_eq!(
        reply.body_json,
        "{\"error\":\"E_RIG_INVALID\",\"detail\":\"no rig\"}"
    );
    let seen = seen.lock().await;
    assert_eq!(seen[0].method, "PUT");
    assert_eq!(seen[0].path, "/api/v1/sensors/rig");
    assert_eq!(seen[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(seen[0].body, b"{\"v\":1}");
}

#[tokio::test]
async fn an_unreachable_control_plane_is_rig_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let host = RealHost::new().with_media_relay(1, dir.path().join("absent.sock"));
    let err = host
        .relay_rig_write(&args(&[("rig_json", "{}")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, HostError::Rpc(m) if m.starts_with("rig_unavailable: control plane:")),
        "{err:?}"
    );
}

#[test]
fn replies_parse_with_a_length_chunks_or_the_close() {
    let r = parse_reply(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nabXX").unwrap();
    assert_eq!((r.status, r.body.as_slice()), (200, &b"ab"[..]));
    let r = parse_reply(b"HTTP/1.1 201 Created\r\nLocation: /a\r\n\r\nrest").unwrap();
    assert_eq!(r.location.as_deref(), Some("/a"));
    assert_eq!(r.body, b"rest");
    let r = parse_reply(
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3;x=1\r\nabc\r\n0\r\n\r\n",
    )
    .unwrap();
    assert_eq!(r.body, b"abc");
    assert!(parse_reply(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab").is_err());
    assert!(parse_reply(b"garbage").is_err());
    assert!(parse_reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nab").is_err());
}

#[test]
fn the_relay_methods_are_gated_on_their_capabilities() {
    use crate::dispatch::{gate, Gate, Method};
    let caps =
        |items: &[&str]| -> BTreeSet<String> { items.iter().map(|s| s.to_string()).collect() };
    for (method, variant) in [
        ("video.whep.offer", Method::WhepOffer),
        ("video.whep.close", Method::WhepClose),
    ] {
        assert_eq!(
            gate(method, false, &caps(&["sensors.rig.write"])),
            Gate::CapabilityDenied("capability_denied: video.whep.relay".to_string())
        );
        assert_eq!(
            gate(method, false, &caps(&["video.whep.relay"])),
            Gate::Allow(variant)
        );
    }
    assert_eq!(
        gate("sensors.rig.write", false, &caps(&["video.whep.relay"])),
        Gate::CapabilityDenied("capability_denied: sensors.rig.write".to_string())
    );
    assert_eq!(
        gate("sensors.rig.write", false, &caps(&["sensors.rig.write"])),
        Gate::Allow(Method::RigWrite)
    );
}
