use ados_protocol::camera_control::{reason, CAMERA_CONTROL_CAPABILITY};
use ados_protocol::frame::{encode_frame, PLUGIN_MAX_FRAME};
use ados_protocol::ipc::IpcBroadcast;
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};

use super::*;

fn temp_sock(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "ados-camctl-test-{}-{name}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// The request a fake engine saw, and its socket, held open until the test
/// takes it so the reply is not cut off.
type Engine = tokio::task::JoinHandle<(Envelope, IpcBroadcast)>;

/// A fake vision engine answering one request with `answer(request)`.
async fn fake_engine(
    name: &str,
    answer: fn(&Envelope) -> (Value, Option<String>),
) -> (RealHost, Engine) {
    let path = temp_sock(name);
    let (server, inbound) = IpcBroadcast::bind(&path, 256, false, Some(16))
        .await
        .unwrap();
    let mut inbound = inbound.expect("inbound channel requested");
    let engine = tokio::spawn(async move {
        let cmd = inbound.recv().await.expect("a host request");
        let req = Envelope::from_msgpack(&cmd.payload).expect("a request envelope");
        let (args, error) = answer(&req);
        let reply = Envelope {
            version: PROTOCOL_VERSION,
            kind: "response".to_string(),
            method: "response".to_string(),
            capability: String::new(),
            args,
            request_id: req.request_id.clone(),
            token: String::new(),
            error,
        };
        let body = reply.to_msgpack().unwrap();
        server
            .broadcast(encode_frame(&body, PLUGIN_MAX_FRAME).unwrap().into())
            .await;
        (req, server)
    });
    let client = VisionClient::spawn_with_interval(path, Duration::from_millis(50));
    assert!(client.connected_within(Duration::from_secs(2)).await);
    (RealHost::new().with_vision(Arc::new(client)), engine)
}

fn request_args(extra: Option<(&str, Value)>) -> Value {
    let mut args = CameraControlRequest {
        camera_id: "cam_down".into(),
        exposure_us: Some(2_000),
        gain: None,
        auto_exposure: Some(false),
    }
    .to_args();
    if let (Value::Map(entries), Some((k, v))) = (&mut args, extra) {
        entries.push((Value::from(k), v));
    }
    args
}

#[tokio::test]
async fn the_request_reaches_the_engine_and_the_read_back_comes_back() {
    let (host, engine) = fake_engine("relay", |_| {
        let result = CameraControlResult {
            applied: true,
            exposure_us: Some(2_000),
            gain: Some(4.0),
            reason: None,
        };
        (result.to_value(), None)
    })
    .await;
    let out = host
        .camera_control_set(
            "p",
            &request_args(Some(("device", Value::from("/dev/video0")))),
        )
        .await
        .unwrap();
    assert_eq!(
        CameraControlResult::from_value(&out).unwrap(),
        CameraControlResult {
            applied: true,
            exposure_us: Some(2_000),
            gain: Some(4.0),
            reason: None,
        }
    );
    let (seen, _socket) = engine.await.unwrap();
    assert_eq!(
        (seen.method.as_str(), seen.capability.as_str()),
        (CAMERA_CONTROL_METHOD, CAMERA_CONTROL_CAPABILITY)
    );
    // Only the declared fields are relayed.
    assert_eq!(seen.args, request_args(None));
}

#[tokio::test]
async fn a_refusal_and_an_engine_error_are_relayed() {
    let (host, engine) = fake_engine("refused", |_| {
        (
            CameraControlResult::refused(reason::NOT_V4L2).to_value(),
            None,
        )
    })
    .await;
    let out = host
        .camera_control_set("p", &request_args(None))
        .await
        .unwrap();
    assert_eq!(
        CameraControlResult::from_value(&out).unwrap(),
        CameraControlResult::refused(reason::NOT_V4L2)
    );
    engine.await.unwrap();

    let (host, engine) = fake_engine("error", |_| {
        (
            Value::Map(vec![]),
            Some("unknown vision method camera.control.set".into()),
        )
    })
    .await;
    let err = host
        .camera_control_set("p", &request_args(None))
        .await
        .unwrap_err();
    assert_eq!(err.body(), "unknown vision method camera.control.set");
    engine.await.unwrap();
}

#[tokio::test]
async fn malformed_arguments_or_no_engine_never_reach_a_camera() {
    let err = RealHost::new()
        .with_vision(Arc::new(VisionClient::spawn_with_interval(
            temp_sock("absent"),
            Duration::from_millis(50),
        )))
        .camera_control_set("p", &Value::Map(vec![]))
        .await
        .unwrap_err();
    assert!(err
        .body()
        .starts_with("camera.control.set arguments are invalid"));

    let out = RealHost::new()
        .camera_control_set("p", &request_args(None))
        .await
        .unwrap();
    let Value::Map(m) = out else { panic!("a map") };
    assert!(m
        .iter()
        .any(|(k, v)| k.as_str() == Some("error") && v.as_str() == Some("not_implemented")));
}
