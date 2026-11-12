use super::*;
use tokio::net::UnixListener;

fn reply(backend: Option<&str>, capable: Option<bool>) -> Value {
    let mut map = Vec::new();
    if let Some(b) = backend {
        map.push((Value::from("backend"), Value::from(b)));
    }
    if let Some(c) = capable {
        map.push((Value::from("backend_inference_capable"), Value::Boolean(c)));
    }
    Value::Map(map)
}

#[test]
fn the_list_models_reply_names_where_the_detector_runs() {
    assert_eq!(
        detector_from_reply(&reply(Some("onnx"), Some(true))),
        LiveDetector::Cpu
    );
    assert_eq!(
        detector_from_reply(&reply(Some("rknn"), Some(true))),
        LiveDetector::Accelerator
    );
    assert_eq!(
        detector_from_reply(&reply(Some("hailo"), Some(true))),
        LiveDetector::Accelerator
    );
    // The mock and an unavailable backend run no real inference.
    assert_eq!(
        detector_from_reply(&reply(Some("mock"), Some(false))),
        LiveDetector::Off
    );
    assert_eq!(
        detector_from_reply(&reply(Some("onnx"), Some(false))),
        LiveDetector::Off
    );
    assert_eq!(
        detector_from_reply(&reply(Some("onnx"), None)),
        LiveDetector::Off
    );
    assert_eq!(detector_from_reply(&Value::Nil), LiveDetector::Off);
}

/// An engine on `path` that answers one `vision.list_models` with `args`.
fn fake_engine(path: &Path, args: Value) -> tokio::task::JoinHandle<String> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await.unwrap();
        let len = decode_len(header, PLUGIN_MAX_FRAME, false).unwrap();
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.unwrap();
        let request = Envelope::from_msgpack(&body).unwrap();
        let response = Envelope {
            kind: "response".to_string(),
            args,
            ..request.clone()
        };
        stream
            .write_all(&response.encode_frame().unwrap())
            .await
            .unwrap();
        request.method
    })
}

#[tokio::test]
async fn the_query_asks_the_engine_and_an_absent_engine_reads_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vision.sock");
    let engine = fake_engine(&path, reply(Some("onnx"), Some(true)));
    assert_eq!(query_live_detector(&path).await, LiveDetector::Cpu);
    assert_eq!(engine.await.unwrap(), methods::LIST_MODELS);

    let absent = dir.path().join("absent.sock");
    assert_eq!(query_live_detector(&absent).await, LiveDetector::Off);
}
