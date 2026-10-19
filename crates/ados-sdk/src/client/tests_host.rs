//! Request/response tests against a scripted host: the tool-invoke reply
//! guarantees and the relay facades' wire shape.

use std::future::Future;

use super::tests::{response, token_string};
use super::*;

/// A scripted host: answers the handshake, then hands the plugin side's
/// stream halves to `script`.
async fn with_host<F, Fut, T>(caps: &[&str], script: F) -> (PluginIpcClient, JoinHandle<T>)
where
    F: FnOnce(OwnedReadHalf, OwnedWriteHalf) -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send,
    T: Send + 'static,
{
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("p.sock");
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
    let server = tokio::spawn(async move {
        let _dir = dir;
        let (stream, _) = listener.accept().await.expect("accept");
        let (mut r, mut w) = stream.into_split();
        let hello = read_envelope(&mut r).await.expect("hello").expect("frame");
        w.write_all(&response(&hello.request_id).encode_frame().unwrap())
            .await
            .expect("ready");
        script(r, w).await
    });
    let client = PluginIpcClient::new("p", token_string(caps, 900).as_str(), &sock);
    (client, server)
}

fn tool_invoke(id: &str, tool: &str) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "request".to_string(),
        method: TOOL_INVOKE_METHOD.to_string(),
        capability: "mcp.expose".to_string(),
        args: Value::Map(vec![(Value::from("tool"), Value::from(tool))]),
        request_id: id.to_string(),
        token: String::new(),
        error: None,
    }
}

/// A handler that panics and a result too large to frame are both answered,
/// so the host never waits out its timeout.
#[tokio::test]
async fn a_panicking_handler_or_an_unframeable_result_still_answers() {
    let (client, server) = with_host(&["mcp.expose"], |mut r, mut w| async move {
        let mut replies = Vec::new();
        for (id, tool) in [("i1", "panic"), ("i2", "huge"), ("i3", "ok")] {
            w.write_all(&tool_invoke(id, tool).encode_frame().unwrap())
                .await
                .expect("invoke");
            replies.push(read_envelope(&mut r).await.expect("read").expect("reply"));
        }
        replies
    })
    .await;
    client.set_tool_handler(Arc::new(|tool: String, _args: Value| {
        Box::pin(async move {
            match tool.as_str() {
                "panic" => panic!("tool bug"),
                "huge" => Ok(Value::Binary(vec![0u8; PLUGIN_MAX_FRAME + 1])),
                _ => Ok(Value::from("fine")),
            }
        })
    }));
    client.connect().await.expect("connect");
    let replies = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("every invocation answered in time")
        .expect("server");
    client.close().await;

    assert_eq!(replies[0].request_id, "i1");
    assert_eq!(replies[0].error.as_deref(), Some("tool_handler_panicked"));
    assert_eq!(replies[1].request_id, "i2");
    assert!(
        replies[1]
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("tool_reply_unencodable:")),
        "{:?}",
        replies[1].error
    );
    assert_eq!(replies[1].args, Value::Map(Vec::new()));
    // The connection and the handler survive both.
    assert_eq!(replies[2].error, None);
    assert_eq!(replies[2].args, Value::from("fine"));
}

/// Answer each request the client sends with `answer(request)`; return the
/// requests seen.
async fn answer_requests(
    mut r: OwnedReadHalf,
    mut w: OwnedWriteHalf,
    n: usize,
    answer: fn(&Envelope) -> Envelope,
) -> Vec<Envelope> {
    let mut seen = Vec::new();
    for _ in 0..n {
        let req = read_envelope(&mut r).await.expect("read").expect("request");
        w.write_all(&answer(&req).encode_frame().unwrap())
            .await
            .expect("reply");
        seen.push(req);
    }
    seen
}

fn reply_to(req: &Envelope, args: Value, error: Option<&str>) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: req.method.clone(),
        capability: req.capability.clone(),
        args,
        request_id: req.request_id.clone(),
        token: String::new(),
        error: error.map(str::to_string),
    }
}

fn kv(entries: &[(&str, Value)]) -> Value {
    Value::Map(
        entries
            .iter()
            .map(|(k, v)| (Value::from(*k), v.clone()))
            .collect(),
    )
}

#[tokio::test]
async fn the_relay_calls_carry_their_methods_capabilities_and_arguments() {
    let (client, server) = with_host(&["video.whep.relay", "sensors.rig.write"], |r, w| {
        answer_requests(r, w, 3, |req| match req.method.as_str() {
            "video.whep.offer" => reply_to(
                req,
                kv(&[
                    ("sdp", Value::from("ANSWER")),
                    ("location", Value::from("/whep/s1?camera=main")),
                ]),
                None,
            ),
            "video.whep.close" => reply_to(req, kv(&[("closed", Value::Boolean(true))]), None),
            _ => reply_to(
                req,
                kv(&[
                    ("status", Value::from(400)),
                    (
                        "body_json",
                        Value::from(r#"{"error":"E_RIG_INVALID","detail":"x"}"#),
                    ),
                ]),
                None,
            ),
        })
    })
    .await;
    client.connect().await.expect("connect");

    let answer = client.whep_offer("main", "OFFER").await.unwrap();
    assert_eq!(
        answer,
        WhepAnswer {
            sdp: "ANSWER".into(),
            location: Some("/whep/s1?camera=main".into()),
        }
    );
    client.whep_close("/whep/s1?camera=main").await.unwrap();
    let rig = serde_json::json!({"rig_id": "r", "sensors": []});
    let (status, body) = client.put_rig(&rig).await.unwrap();
    assert_eq!(status, 400);
    assert_eq!(body["error"], "E_RIG_INVALID");

    let seen = server.await.unwrap();
    client.close().await;
    assert_eq!(
        (seen[0].method.as_str(), seen[0].capability.as_str()),
        ("video.whep.offer", "video.whep.relay")
    );
    assert_eq!(map_get_str(&seen[0].args, "camera_id"), Some("main"));
    assert_eq!(map_get_str(&seen[0].args, "sdp"), Some("OFFER"));
    assert_eq!(
        (seen[1].method.as_str(), seen[1].capability.as_str()),
        ("video.whep.close", "video.whep.relay")
    );
    assert_eq!(
        map_get_str(&seen[1].args, "location"),
        Some("/whep/s1?camera=main")
    );
    assert_eq!(
        (seen[2].method.as_str(), seen[2].capability.as_str()),
        ("sensors.rig.write", "sensors.rig.write")
    );
    let sent: serde_json::Value =
        serde_json::from_str(map_get_str(&seen[2].args, "rig_json").unwrap()).unwrap();
    assert_eq!(sent, rig);
}

#[tokio::test]
async fn relay_refusals_surface_as_typed_errors() {
    let (client, server) = with_host(&[], |r, w| {
        answer_requests(r, w, 3, |req| match req.method.as_str() {
            "video.whep.offer" => reply_to(
                req,
                Value::Map(vec![]),
                Some("capability_denied: video.whep.relay"),
            ),
            "video.whep.close" => reply_to(
                req,
                Value::Map(vec![]),
                Some("whep_unavailable: unknown location: not a session this plugin opened"),
            ),
            _ => reply_to(
                req,
                Value::Map(vec![]),
                Some("rig_unavailable: control plane: connection refused"),
            ),
        })
    })
    .await;
    client.connect().await.expect("connect");
    assert!(matches!(
        client.whep_offer("main", "o").await,
        Err(ClientError::CapabilityDenied(c)) if c == "video.whep.relay"
    ));
    assert!(matches!(
        client.whep_close("/whep/x?camera=main").await,
        Err(ClientError::Rpc(m)) if m.starts_with("whep_unavailable:")
    ));
    assert!(matches!(
        client.put_rig(&serde_json::json!({})).await,
        Err(ClientError::Rpc(m)) if m.starts_with("rig_unavailable:")
    ));
    server.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn camera_controls_carry_the_request_and_decode_the_read_back() {
    use ados_protocol::camera_control::{CameraControlRequest, CameraControlResult};
    let (client, server) = with_host(&["camera.control.write"], |r, w| {
        answer_requests(r, w, 3, |req| match map_get(&req.args, "camera_id") {
            Some(id) if id.as_str() == Some("cam_down") => reply_to(
                req,
                kv(&[
                    ("applied", Value::Boolean(true)),
                    ("exposure_us", Value::from(2_000)),
                    ("gain", Value::from(4.0)),
                ]),
                None,
            ),
            Some(id) if id.as_str() == Some("cam_fl") => reply_to(
                req,
                kv(&[
                    ("applied", Value::Boolean(false)),
                    ("reason", Value::from("not_v4l2")),
                ]),
                None,
            ),
            _ => reply_to(
                req,
                Value::Map(vec![]),
                Some("capability_denied: camera.control.write"),
            ),
        })
    })
    .await;
    client.connect().await.expect("connect");

    let req = CameraControlRequest {
        camera_id: "cam_down".into(),
        exposure_us: Some(2_000),
        gain: Some(4.0),
        auto_exposure: None,
    };
    assert_eq!(
        client.camera_set_controls(&req).await.unwrap(),
        CameraControlResult {
            applied: true,
            exposure_us: Some(2_000),
            gain: Some(4.0),
            reason: None,
        }
    );
    let sim = CameraControlRequest {
        camera_id: "cam_fl".into(),
        auto_exposure: Some(true),
        ..Default::default()
    };
    assert_eq!(
        client.camera_set_controls(&sim).await.unwrap(),
        CameraControlResult::refused("not_v4l2")
    );
    let other = CameraControlRequest {
        camera_id: "x".into(),
        ..Default::default()
    };
    assert!(matches!(
        client.camera_set_controls(&other).await,
        Err(ClientError::CapabilityDenied(c)) if c == "camera.control.write"
    ));

    let seen = server.await.unwrap();
    client.close().await;
    assert_eq!(
        (seen[0].method.as_str(), seen[0].capability.as_str()),
        ("camera.control.set", "camera.control.write")
    );
    assert_eq!(CameraControlRequest::from_args(&seen[0].args).unwrap(), req);
    assert_eq!(CameraControlRequest::from_args(&seen[1].args).unwrap(), sim);
}
