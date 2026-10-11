use super::sidecar::{map_get, mv};
use super::*;
use ados_protocol::framebus::{ModelExecution, ModelKind};

fn meta() -> ModelMetadata {
    ModelMetadata {
        id: "com.example.test".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 480,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["a".into()],
        model_path: None,
        head: ados_protocol::framebus::DetectionHead::Yolo8,
    }
}

#[test]
fn mock_backend_loads_and_infers_empty() {
    let b = MockBackend;
    assert_eq!(b.name(), "mock");
    let m = b.load(&meta()).unwrap();
    let out = m.infer(&[0u8; 12], 2, 2, FrameFormat::Rgb24).unwrap();
    assert!(out.is_empty());
}

#[test]
fn rknn_backend_records_socket_and_errors_without_sidecar() {
    let b = SidecarBackend::new("/nonexistent/ados-vision-rknn.sock", "rknn");
    assert_eq!(b.name(), "rknn");
    assert_eq!(b.socket_path(), "/nonexistent/ados-vision-rknn.sock");
    let m = b.load(&meta()).unwrap();
    // No sidecar at the socket ⇒ infer returns Err (the engine degrades to the
    // mock model for this registration), never panics.
    assert!(m.infer(&[0u8; 4], 1, 1, FrameFormat::Rgb24).is_err());
}

#[test]
fn rknn_infer_round_trips_with_mock_sidecar() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("rknn.sock");
    let listener = UnixListener::bind(&sock).unwrap();

    // A mock sidecar: answer the load handshake, then return one detection for infer.
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().unwrap();
            let mut len = [0u8; 4];
            conn.read_exact(&mut len).unwrap();
            let n = u32::from_be_bytes(len) as usize;
            let mut buf = vec![0u8; n];
            conn.read_exact(&mut buf).unwrap();
            let req = rmpv::decode::read_value(&mut &buf[..]).unwrap();
            let op = req
                .as_map()
                .and_then(|m| map_get(m, "op"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let resp = if op == "infer" {
                let det = rmpv::Value::Map(vec![
                    (
                        mv("bbox"),
                        rmpv::Value::Map(vec![
                            (mv("x"), rmpv::Value::F64(10.0)),
                            (mv("y"), rmpv::Value::F64(20.0)),
                            (mv("width"), rmpv::Value::F64(30.0)),
                            (mv("height"), rmpv::Value::F64(40.0)),
                        ]),
                    ),
                    (mv("class_label"), mv("UAV")),
                    (mv("confidence"), rmpv::Value::F64(0.9)),
                    (mv("track_id"), rmpv::Value::from(7u64)),
                    (mv("lock_state"), mv("locked")),
                ]);
                rmpv::Value::Map(vec![
                    (mv("status"), mv("ok")),
                    (mv("detections"), rmpv::Value::Array(vec![det])),
                ])
            } else {
                rmpv::Value::Map(vec![(mv("status"), mv("ok"))])
            };
            let mut body = Vec::new();
            rmpv::encode::write_value(&mut body, &resp).unwrap();
            conn.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
            conn.write_all(&body).unwrap();
            conn.flush().unwrap();
        }
    });

    let backend = SidecarBackend::new(sock.to_str().unwrap(), "rknn");
    let mut m = meta();
    m.model_path = Some("/tmp/uav.rknn".into());
    let model = backend.load(&m).unwrap();
    let dets = model
        .infer(&[1, 2, 3, 4], 1, 1, FrameFormat::Rgb24)
        .unwrap();
    server.join().unwrap();

    assert_eq!(dets.len(), 1);
    assert_eq!(dets[0].class_label, "UAV");
    assert_eq!(dets[0].track_id, Some(7));
    assert_eq!(dets[0].lock_state, Some(LockState::Locked));
    let b0 = dets[0].bbox.as_ref().unwrap();
    assert!((b0.x - 10.0).abs() < 1e-3);
    assert!((b0.height - 40.0).abs() < 1e-3);
    assert!((dets[0].confidence - 0.9).abs() < 1e-3);
}

#[test]
fn picker_auto_selects_sidecar_on_rockchip() {
    let prefs = BackendPrefs {
        preference: "auto",
        rknn_socket_path: "/run/ados/vision-rknn.sock".into(),
        hailo_socket_path: "/run/ados/vision-hailo.sock".into(),
        mock_target: false,
    };
    assert_eq!(select_backend("rk3576", &prefs).name(), "rknn");
    assert_eq!(select_backend("RK3588S2", &prefs).name(), "rknn");
    // A Jetson has no accelerated backend: auto must not route it to the
    // RKNN sidecar, which cannot run there.
    assert_ne!(select_backend("tegra234", &prefs).name(), "rknn");
    // A CPU-only SoC under auto prefers the real ONNX CPU backend when it is
    // compiled in, and only falls back to the detection-less mock when no
    // runtime is available. Either way it never silently picks mock when a
    // real CPU backend exists.
    let cpu = select_backend("bcm2711", &prefs);
    #[cfg(feature = "onnx")]
    {
        assert_eq!(cpu.name(), "onnx");
        assert!(cpu.is_inference_capable());
    }
    #[cfg(not(feature = "onnx"))]
    {
        assert_eq!(cpu.name(), "mock");
        // The mock is honestly flagged as not running real inference.
        assert!(!cpu.is_inference_capable());
    }
}

#[test]
fn a_sidecar_that_is_not_answering_is_not_inference_capable() {
    // The status surface keys on this: an unreachable sidecar means a
    // permanently empty detection stream, so reporting it capable made
    // /api/status advertise perception_tier "local" for an accelerator that
    // was not there.
    assert!(!MockBackend.is_inference_capable());
    assert!(
        !SidecarBackend::new("/nonexistent/ados-vision-rknn.sock", "rknn").is_inference_capable()
    );

    // The residue case: a sidecar killed with SIGKILL leaves its socket
    // inode behind, so an existence check would report this as healthy.
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join("rknn.sock");
    std::fs::write(&stale, b"").unwrap();
    assert!(!SidecarBackend::new(stale.to_str().unwrap(), "rknn").is_inference_capable());
}

#[test]
fn a_sidecar_that_dies_stops_reporting_capable_on_the_failed_exchange() {
    use std::os::unix::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("rknn.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let backend = SidecarBackend::new(sock.to_str().unwrap(), "rknn");
    assert!(
        backend.is_inference_capable(),
        "a sidecar that is listening is capable"
    );
    let model = backend.load(&meta()).unwrap();

    // The sidecar goes away mid-flight, socket and all.
    drop(listener);
    std::fs::remove_file(&sock).unwrap();
    assert!(model.infer(&[0u8; 4], 1, 1, FrameFormat::Rgb24).is_err());

    // A failed exchange is stronger evidence than the cached probe, so the
    // capability flips on the failure rather than after the liveness TTL.
    assert!(!backend.is_inference_capable());
}

#[test]
fn picker_honours_explicit_preference() {
    let prefs = BackendPrefs {
        preference: "mock",
        rknn_socket_path: "/run/ados/vision-rknn.sock".into(),
        hailo_socket_path: "/run/ados/vision-hailo.sock".into(),
        mock_target: false,
    };
    assert_eq!(select_backend("rk3576", &prefs).name(), "mock");

    // onnx falls back to mock when not compiled in.
    let prefs_onnx = BackendPrefs {
        preference: "onnx",
        rknn_socket_path: "/run/ados/vision-rknn.sock".into(),
        hailo_socket_path: "/run/ados/vision-hailo.sock".into(),
        mock_target: false,
    };
    let name = select_backend("x86", &prefs_onnx).name().to_string();
    #[cfg(feature = "onnx")]
    assert_eq!(name, "onnx");
    #[cfg(not(feature = "onnx"))]
    assert_eq!(name, "mock");
}

#[test]
fn mock_target_orbits() {
    let prefs = BackendPrefs {
        preference: "mock",
        rknn_socket_path: "/run/ados/vision-rknn.sock".into(),
        hailo_socket_path: "/run/ados/vision-hailo.sock".into(),
        mock_target: true,
    };
    let backend = select_backend("x86", &prefs);
    assert_eq!(backend.name(), "mock");
    assert!(!backend.is_inference_capable());
    let model = backend.load(&meta()).unwrap();
    let dets = model
        .infer(&[0u8; 12], 1000, 500, FrameFormat::Rgb24)
        .unwrap();
    assert_eq!(dets.len(), 1);
    assert_eq!(dets[0].class_label, "person");
    assert!((dets[0].confidence - 0.9).abs() < 1e-6);
    assert_eq!(dets[0].track_id, Some(MOCK_TARGET_TRACK_ID));

    let centre = |d: &Detection| {
        let b = d.bbox.unwrap();
        (
            (b.x + b.width / 2.0) / 1000.0,
            (b.y + b.height / 2.0) / 500.0,
        )
    };
    // t = 0: on the +x side of the orbit; a quarter period (2.5 s) later on +y.
    let d0 = mock_target_at(0.0, 1000, 500);
    let (x0, y0) = centre(&d0);
    assert!((x0 - 0.75).abs() < 1e-4 && (y0 - 0.5).abs() < 1e-4);
    let b0 = d0.bbox.unwrap();
    assert!((b0.width - 120.0).abs() < 1e-3 && (b0.height - 150.0).abs() < 1e-3);
    let (x1, y1) = centre(&mock_target_at(2.5, 1000, 500));
    assert!((x1 - 0.5).abs() < 1e-4 && (y1 - 0.75).abs() < 1e-4);
    // Every point stays on the radius-0.25 circle with the same track id.
    for i in 0..40 {
        let d = mock_target_at(f64::from(i) * 0.37, 1000, 500);
        let (x, y) = centre(&d);
        let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        assert!((r - 0.25).abs() < 1e-4);
        assert_eq!(d.track_id, Some(MOCK_TARGET_TRACK_ID));
    }

    // Without the flag the mock stays empty.
    let plain = BackendPrefs {
        mock_target: false,
        ..prefs
    };
    let m = select_backend("x86", &plain).load(&meta()).unwrap();
    assert!(m
        .infer(&[0u8; 12], 2, 2, FrameFormat::Rgb24)
        .unwrap()
        .is_empty());
}
