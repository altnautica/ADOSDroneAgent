use super::net::find_subslice;
use super::*;
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn default_params<'a>(lan_ips: &'a [String], streams: &'a [(String, String)]) -> ConfigParams<'a> {
    ConfigParams::new(MediamtxProfile::Air, lan_ips, streams)
}

// --- config field + value parity ----------------------------------

#[test]
fn config_has_exact_ports_and_main_path() {
    let lan = vec!["192.168.1.115".to_string()];
    let streams = vec![("main".to_string(), "publisher".to_string())];
    let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));

    // Parse back and assert the structure rather than string-matching the
    // whole document (mediamtx, not the test, defines acceptable YAML).
    let v: Value = serde_norway::from_str(&yaml).unwrap();

    assert_eq!(v["apiAddress"], "127.0.0.1:9997");
    assert_eq!(v["rtspAddress"], ":8554");
    assert_eq!(v["webrtcAddress"], "127.0.0.1:8889");
    assert_eq!(v["hlsAddress"], "127.0.0.1:8888");
    assert_eq!(v["api"], true);
    assert_eq!(v["rtsp"], true);
    // UDP RTSP transport disabled: mediamtx's own default `rtpAddress`/
    // `rtcpAddress` (:8000/:8001) collide with ados-radio's wfb_tx
    // control-port range.
    assert_eq!(v["rtspTransports"], Value::from(vec!["tcp"]));
    assert_eq!(v["webrtc"], true);
    assert_eq!(v["hls"], true);
    assert_eq!(v["logLevel"], "warn");
    assert_eq!(v["webrtcAllowOrigin"], "*");
    // Upstream's own default. The 15s both profiles carried bought nothing.
    assert_eq!(v["webrtcHandshakeTimeout"], "10s");
    assert_eq!(v["webrtcSTUNGatherTimeout"], "2s");
    // Pinned at upstream's default, NOT tightened to 5s: the ground rig
    // measured a 5s ceiling producing a deterministic 120s publisher
    // eviction cycle on Pi 4B 1 GB boards under swap pressure.
    assert_eq!(v["readTimeout"], "10s");
    assert_eq!(v["writeTimeout"], "10s");
    // Burst tolerance, raised off upstream's 512 on BOTH profiles: the
    // ground observed 512 evicting browser WHEP readers, and the air side
    // serves the same readers.
    assert_eq!(v["writeQueueSize"], 4096);
    assert_eq!(v["udpMaxPayloadSize"], 1452);
    // The playback server is the read side of the native recorder, and it
    // listens even when `record` is off so already-captured segments stay
    // exportable.
    assert_eq!(v["playback"], true);
    assert_eq!(v["playbackAddress"], "127.0.0.1:9996");

    // WebRTC media binds all interfaces (:8189); ICE host candidates are
    // gathered from the physical interfaces per session so the media path
    // follows an interface/IP change. The interface list is read from
    // /sys/class/net at runtime, so assert only that it is a sequence (its
    // contents depend on the host running the test).
    assert_eq!(v["webrtcIPsFromInterfaces"], true);
    assert!(v["webrtcIPsFromInterfacesList"].as_array().is_some());
    assert_eq!(v["webrtcLocalUDPAddress"], ":8189");
    // Empty on the LAN/direct profile (upstream's own default): a winning
    // TCP ICE candidate on a lossy air link turns FEC-tolerant loss into
    // head-of-line-blocked latency that never recovers.
    assert_eq!(v["webrtcLocalTCPAddress"], "");
    assert_eq!(
        v["webrtcAdditionalHosts"],
        Value::from(vec!["192.168.1.115"])
    );

    // HLS low-latency, 7 segs x 1s, always remux.
    assert_eq!(v["hlsAlwaysRemux"], false);
    assert_eq!(v["hlsVariant"], "lowLatency");
    assert_eq!(v["hlsSegmentCount"], 7);
    assert_eq!(v["hlsSegmentDuration"], "1s");
    assert_eq!(v["hlsAllowOrigin"], "*");
    // The LL-HLS latency floor, pinned rather than inherited.
    assert_eq!(v["hlsPartDuration"], "200ms");

    // The `main` publisher path: source=publisher, no sourceOnDemand.
    assert_eq!(v["paths"]["main"]["source"], "publisher");
    assert!(v["paths"]["main"].get("sourceOnDemand").is_none());
}

/// STUN is cloud-relay-only. On a LAN session that connects on host
/// candidates every entry is pure pre-first-frame handshake latency.
#[test]
fn lan_profiles_carry_no_stun_and_cloud_relay_carries_all_five() {
    let lan: Vec<String> = vec![];
    let streams = vec![("main".to_string(), "publisher".to_string())];

    for profile in [MediamtxProfile::Air, MediamtxProfile::Ground] {
        let yaml = mediamtx_config_yaml(&ConfigParams::new(profile, &lan, &streams));
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(
            v["webrtcICEServers2"].as_array().unwrap().len(),
            0,
            "{profile:?} is a LAN/direct profile and must gather no STUN"
        );
        assert_eq!(
            v["webrtcLocalTCPAddress"], "",
            "{profile:?} must not offer a TCP ICE candidate"
        );
    }

    let yaml = mediamtx_config_yaml(&ConfigParams::new(
        MediamtxProfile::CloudRelay,
        &lan,
        &streams,
    ));
    let v: Value = serde_norway::from_str(&yaml).unwrap();
    let urls: Vec<&str> = v["webrtcICEServers2"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        urls,
        vec![
            "stun:stun.l.google.com:19302",
            "stun:stun1.l.google.com:19302",
            "stun:stun2.l.google.com:19302",
            "stun:stun.cloudflare.com:3478",
            "stun:global.stun.twilio.com:3478",
        ]
    );
    // A relay may face a UDP-hostile network, so TCP is a legitimate last
    // resort there and only there.
    assert_eq!(v["webrtcLocalTCPAddress"], ":8189");
}

/// THE PARITY GATE.
///
/// The air and ground mediamtx configs were rendered by two independent
/// generators that had silently drifted apart — `writeQueueSize` absent on
/// air and 4096 on ground, `udpMaxPayloadSize` absent on air and 1472 on
/// ground — and nothing could see it. Every key below is genuinely shared:
/// both profiles serve the same browser WHEP readers off the same code
/// path, so a difference is drift, not a decision.
///
/// The two keys that ARE allowed to differ (`hlsVariant`,
/// `webrtcLocalTCPAddress` / `webrtcICEServers2`) are asserted to differ
/// elsewhere, so this list staying exhaustive is what makes the gate real:
/// a new key added to one arm of the profile `match` without a stated
/// reason fails here.
#[test]
fn air_and_ground_agree_on_every_shared_key() {
    let lan = vec!["192.168.1.50".to_string()];
    let streams = vec![("main".to_string(), "publisher".to_string())];

    let air: Value = serde_norway::from_str(&mediamtx_config_yaml(&ConfigParams::new(
        MediamtxProfile::Air,
        &lan,
        &streams,
    )))
    .unwrap();
    let ground: Value = serde_norway::from_str(&mediamtx_config_yaml(&ConfigParams::new(
        MediamtxProfile::Ground,
        &lan,
        &streams,
    )))
    .unwrap();

    const SHARED_KEYS: &[&str] = &[
        "logLevel",
        "api",
        "apiAddress",
        "readTimeout",
        "writeTimeout",
        "writeQueueSize",
        "udpMaxPayloadSize",
        "playback",
        "playbackAddress",
        "rtsp",
        "rtspAddress",
        "rtspTransports",
        "rtmp",
        "srt",
        "webrtc",
        "webrtcAddress",
        "webrtcAllowOrigin",
        "webrtcIPsFromInterfaces",
        "webrtcIPsFromInterfacesList",
        "webrtcHandshakeTimeout",
        "webrtcSTUNGatherTimeout",
        "webrtcLocalUDPAddress",
        "webrtcAdditionalHosts",
        "hls",
        "hlsAddress",
        "hlsAlwaysRemux",
        "hlsSegmentCount",
        "hlsSegmentDuration",
        "hlsPartDuration",
        "hlsAllowOrigin",
        "pathDefaults",
        "authInternalUsers",
        "paths",
    ];

    for key in SHARED_KEYS {
        assert_eq!(
            air[key], ground[key],
            "air and ground diverge on the shared key `{key}`: \
                 air={:?} ground={:?}. Either it is genuinely per-profile \
                 (put it behind a MediamtxProfile method with a stated reason \
                 and drop it from SHARED_KEYS) or this is drift.",
            air[key], ground[key]
        );
    }

    // Every key either agrees above or is a declared per-profile
    // difference. Nothing may be silently absent from both lists.
    const PER_PROFILE_KEYS: &[&str] = &["hlsVariant", "webrtcLocalTCPAddress", "webrtcICEServers2"];
    let rendered: Vec<String> = air
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.to_string())
        .collect();
    for key in &rendered {
        assert!(
            SHARED_KEYS.contains(&key.as_str()) || PER_PROFILE_KEYS.contains(&key.as_str()),
            "`{key}` is rendered but classified neither shared nor \
                 per-profile, so the parity gate does not cover it"
        );
    }
    assert_ne!(
        air["hlsVariant"], ground["hlsVariant"],
        "the ground station is pinned to mpegts for measured reasons; if \
             that changed, move hlsVariant into SHARED_KEYS"
    );
}

/// The media server never faces the network over HTTP. The agent's front
/// credential-gates `/whep` and `/hls`; an HTTP listener on all interfaces
/// would serve the same streams, the recordings and the control API one
/// port away with no credential. Only the RTSP listener (an offload node
/// pulls the feed) and the ICE media socket stay on all interfaces, and a
/// LAN peer may read but never publish.
#[test]
fn http_listeners_bind_loopback_and_publish_is_loopback_only() {
    let lan = vec!["192.168.1.50".to_string()];
    let streams = vec![("main".to_string(), "publisher".to_string())];
    for profile in [
        MediamtxProfile::Air,
        MediamtxProfile::Ground,
        MediamtxProfile::CloudRelay,
    ] {
        let v: Value = serde_norway::from_str(&mediamtx_config_yaml(&ConfigParams::new(
            profile, &lan, &streams,
        )))
        .unwrap();
        for key in [
            "apiAddress",
            "webrtcAddress",
            "hlsAddress",
            "playbackAddress",
        ] {
            let addr = v[key].as_str().unwrap();
            assert!(
                addr.starts_with("127.0.0.1:"),
                "{profile:?} {key}={addr} must bind loopback"
            );
        }
        // Metrics and pprof are HTTP listeners too; they stay off.
        assert!(v.get("metrics").is_none() && v.get("pprof").is_none());
        // RTMP and SRT are unused and would face the network.
        assert_eq!(v["rtmp"], false);
        assert_eq!(v["srt"], false);
        assert_eq!(v["webrtcLocalUDPAddress"], ":8189");

        let users = v["authInternalUsers"].as_array().unwrap();
        let actions = |u: &Value| -> Vec<String> {
            u["permissions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["action"].as_str().unwrap().to_string())
                .collect()
        };
        for u in users {
            assert_eq!(u["user"], "any");
            let ips: Vec<&str> = u["ips"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_str().unwrap())
                .collect();
            let from_anywhere = ips.is_empty();
            if from_anywhere {
                assert_eq!(
                    actions(u),
                    vec!["read"],
                    "{profile:?}: off-box may only read"
                );
            } else {
                assert_eq!(ips, vec!["127.0.0.1", "::1"]);
            }
        }
        assert!(
            users
                .iter()
                .any(|u| actions(u).contains(&"publish".to_string())),
            "{profile:?}: the local encoder must still be able to publish"
        );
    }
}

/// The native fMP4 recorder must render on BOTH profiles when enabled: the
/// drone had no recording of any kind and the ground station had a single
/// whole-stream `+faststart` MP4 whose moov atom is written only on a clean
/// exit — so a power cut mid-flight yielded a zero-recoverable file.
#[test]
fn recording_renders_the_fmp4_block_and_playback_on_both_profiles() {
    let lan: Vec<String> = vec![];
    let streams = vec![("main".to_string(), "publisher".to_string())];

    for profile in [MediamtxProfile::Air, MediamtxProfile::Ground] {
        let mut params = ConfigParams::new(profile, &lan, &streams);
        params.recording = RecordingParams {
            enabled: true,
            ..RecordingParams::default()
        };
        let v: Value = serde_norway::from_str(&mediamtx_config_yaml(&params)).unwrap();
        let pd = &v["pathDefaults"];

        assert_eq!(pd["record"], true, "{profile:?} must record");
        assert_eq!(pd["recordFormat"], "fmp4", "{profile:?} must be fMP4");
        // The part duration IS the recovery point objective: each part is
        // independently flushed, so an unclean exit loses at most this much.
        assert_eq!(pd["recordPartDuration"], "200ms");
        assert_eq!(pd["recordMaxPartSize"], "50M");
        // One file per minute bounds the blast radius of a corrupt file.
        assert_eq!(pd["recordSegmentDuration"], "60s");
        assert_eq!(pd["recordDeleteAfter"], "24h");
        assert_eq!(
            pd["recordPath"], "/var/ados/recordings/%path/%Y-%m-%d_%H-%M-%S-%f",
            "%f is what keeps two segments opened in the same second apart"
        );
        // Without the playback server the segments are unreachable, so
        // recording without it is a write-only store.
        assert_eq!(v["playback"], true);
        assert_eq!(v["playbackAddress"], "127.0.0.1:9996");
        // Off: with it on, mediamtx drops every RTP packet until the
        // encoder's first RTCP sender report (~5 s after each spawn), which
        // blanked `main` on every encoder respawn and profile switch.
        // Capture time rides in the injected SEI instead.
        assert_eq!(pd["useAbsoluteTimestamp"], false);
        // No hook: the reclaim owner is the supervisor janitor, and a
        // second reclaimer would race it to delete the same files.
        assert!(pd.get("runOnRecordSegmentComplete").is_none());
    }
}

/// Recording off must still leave the block renderable and playback up —
/// an operator who just stopped recording still wants the last flight.
#[test]
fn recording_disabled_keeps_playback_and_sets_record_false() {
    let lan: Vec<String> = vec![];
    let streams = vec![("main".to_string(), "publisher".to_string())];
    let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
    let v: Value = serde_norway::from_str(&yaml).unwrap();
    assert_eq!(v["pathDefaults"]["record"], false);
    assert_eq!(v["playback"], true);
}

#[test]
fn config_without_lan_ip_falls_back_and_omits_additional_hosts() {
    let lan: Vec<String> = vec![];
    let streams = vec![("main".to_string(), "publisher".to_string())];
    let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
    let v: Value = serde_norway::from_str(&yaml).unwrap();
    // No LAN IP → UDP/TCP fall back to :8189 and additional-hosts is absent.
    assert_eq!(v["webrtcLocalUDPAddress"], ":8189");
    assert_eq!(v["webrtcLocalTCPAddress"], "");
    assert!(v.get("webrtcAdditionalHosts").is_none());
}

#[test]
fn non_publisher_source_gets_source_on_demand() {
    let lan: Vec<String> = vec![];
    let streams = vec![
        ("main".to_string(), "publisher".to_string()),
        ("cam2".to_string(), "rtsp://10.0.0.9:554/live".to_string()),
    ];
    let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
    let v: Value = serde_norway::from_str(&yaml).unwrap();
    assert!(v["paths"]["main"].get("sourceOnDemand").is_none());
    assert_eq!(v["paths"]["cam2"]["sourceOnDemand"], true);
    assert_eq!(v["paths"]["cam2"]["source"], "rtsp://10.0.0.9:554/live");
}

#[test]
fn config_round_trips_through_typed_map() {
    // Sanity: the rendered YAML deserializes into a typed paths map.
    let lan = vec!["10.1.1.5".to_string()];
    let streams = vec![("main".to_string(), "publisher".to_string())];
    let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
    #[derive(serde::Deserialize)]
    struct Probe {
        paths: BTreeMap<String, Value>,
    }
    let probe: Probe = serde_norway::from_str(&yaml).unwrap();
    assert!(probe.paths.contains_key("main"));
}

// --- write_config I/O ----------------------------------------------

#[tokio::test]
async fn write_config_creates_file() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = MediamtxManager::new(dir.path());
    let streams = vec![("main".to_string(), "publisher".to_string())];
    mgr.write_config_with_ips(&streams, &["192.168.1.50".to_string()])
        .await
        .unwrap();
    assert!(mgr.config_path().exists());
    let text = std::fs::read_to_string(mgr.config_path()).unwrap();
    let v: Value = serde_norway::from_str(&text).unwrap();
    assert_eq!(v["rtspAddress"], ":8554");
    assert_eq!(v["paths"]["main"]["source"], "publisher");
}

/// The recording knobs must survive the manager → renderer → disk path,
/// not just the pure renderer: `video.recording` was dead config for its
/// whole life and a wiring break is exactly how it stays dead.
#[tokio::test]
async fn write_config_carries_the_recording_knobs_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = MediamtxManager::for_profile(MediamtxProfile::Ground, dir.path()).with_recording(
        RecordingParams {
            enabled: true,
            root: "/var/ados/recordings".into(),
            delete_after: "6h".into(),
            ..RecordingParams::default()
        },
    );
    mgr.write_config_with_ips(&[("main".to_string(), "publisher".to_string())], &[])
        .await
        .unwrap();
    let text = std::fs::read_to_string(mgr.config_path()).unwrap();
    let v: Value = serde_norway::from_str(&text).unwrap();
    assert_eq!(v["pathDefaults"]["record"], true);
    assert_eq!(v["pathDefaults"]["recordDeleteAfter"], "6h");
    assert_eq!(v["pathDefaults"]["recordFormat"], "fmp4");
    assert_eq!(v["playbackAddress"], "127.0.0.1:9996");
}

// --- HTTP client against an in-test listener -----------------------

/// Serve exactly one canned HTTP response on a fresh loopback listener and
/// return the chosen port.
async fn serve_once(response: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            // Drain the request (read once is enough for a small GET).
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
            // Drop closes the connection → read_to_EOF on the client side.
        }
    });
    port
}

#[tokio::test]
async fn path_ready_parses_true() {
    let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                    {\"name\":\"main\",\"ready\":true,\"bytesReceived\":4096}";
    let port = serve_once(resp).await;
    let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
        port,
        DEFAULT_RTSP_PORT,
        DEFAULT_WEBRTC_PORT,
        DEFAULT_HLS_PORT,
    );
    assert!(mgr.path_ready("main").await);
}

#[tokio::test]
async fn path_ready_parses_false() {
    let resp = "HTTP/1.1 200 OK\r\n\r\n{\"ready\":false}";
    let port = serve_once(resp).await;
    let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
        port,
        DEFAULT_RTSP_PORT,
        DEFAULT_WEBRTC_PORT,
        DEFAULT_HLS_PORT,
    );
    assert!(!mgr.path_ready("main").await);
}

#[tokio::test]
async fn inbound_bytes_parses_counter() {
    let resp = "HTTP/1.1 200 OK\r\n\r\n{\"ready\":true,\"bytesReceived\":123456}";
    let port = serve_once(resp).await;
    let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
        port,
        DEFAULT_RTSP_PORT,
        DEFAULT_WEBRTC_PORT,
        DEFAULT_HLS_PORT,
    );
    assert_eq!(mgr.inbound_bytes("main").await, Some(123456));
}

#[tokio::test]
async fn inbound_bytes_missing_field_is_none() {
    let resp = "HTTP/1.1 200 OK\r\n\r\n{\"ready\":true}";
    let port = serve_once(resp).await;
    let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
        port,
        DEFAULT_RTSP_PORT,
        DEFAULT_WEBRTC_PORT,
        DEFAULT_HLS_PORT,
    );
    assert_eq!(mgr.inbound_bytes("main").await, None);
}

#[tokio::test]
async fn non_200_is_not_ready_and_no_bytes() {
    let resp = "HTTP/1.1 404 Not Found\r\n\r\n{\"error\":\"not found\"}";
    let port = serve_once(resp).await;
    let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
        port,
        DEFAULT_RTSP_PORT,
        DEFAULT_WEBRTC_PORT,
        DEFAULT_HLS_PORT,
    );
    assert!(!mgr.path_ready("main").await);
    assert_eq!(mgr.inbound_bytes("main").await, None);
}

#[tokio::test]
async fn connection_refused_is_graceful() {
    // Bind a listener to grab a free port, then drop it so the port is
    // (almost certainly) closed — a connect there is refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
        port,
        DEFAULT_RTSP_PORT,
        DEFAULT_WEBRTC_PORT,
        DEFAULT_HLS_PORT,
    );
    assert!(!mgr.path_ready("main").await);
    assert_eq!(mgr.inbound_bytes("main").await, None);
}

#[tokio::test]
async fn wait_for_tcp_port_succeeds_on_open_listener() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Keep the listener alive for the probe.
    let ok = wait_for_tcp_port("127.0.0.1", port, Duration::from_secs(2)).await;
    assert!(ok);
    drop(listener);
}

#[tokio::test]
async fn wait_for_tcp_port_times_out_on_closed_port() {
    // Bind an ephemeral port, then drop the listener so the port is closed.
    // Under the parallel test runner a concurrent bind can momentarily reuse
    // that just-freed port number inside the probe window, so retry on a fresh
    // port rather than flaking — five independent ports all being reused at
    // once is not a real outcome.
    for _ in 0..5 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        if !wait_for_tcp_port("127.0.0.1", port, Duration::from_millis(300)).await {
            return; // closed as expected
        }
    }
    panic!("every freed ephemeral port probed as open — port reuse, not a real reachable port");
}

#[test]
fn find_subslice_basic() {
    assert_eq!(find_subslice(b"abc\r\n\r\nxyz", b"\r\n\r\n"), Some(3));
    assert_eq!(find_subslice(b"no-sep-here", b"\r\n\r\n"), None);
    assert_eq!(find_subslice(b"", b"x"), None);
}
