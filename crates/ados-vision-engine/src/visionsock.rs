//! The `/run/ados/vision.sock` request/response server.
//!
//! The engine serves this socket; the plugin host is the only client. It speaks
//! the same wire as the plugin RPC socket: 4-byte big-endian length-prefixed
//! msgpack [`ados_protocol::plugin::Envelope`] frames (zero-length rejected).
//! The host has already gated each call on the matching vision capability before
//! it reaches this socket, so the server does not re-check tokens; being on the
//! socket is the authorization.
//!
//! Request methods (the [`ados_protocol::framebus::methods`] constants carried
//! in `Envelope::method`):
//!
//! - `vision.subscribe_frames` — start streaming frame descriptors to this
//!   connection. Every published descriptor is pushed as a `vision.deliver`
//!   event envelope whose `args` map carries the encoded descriptor as a binary
//!   `descriptor` field (the host fans these out to subscribed plugins).
//! - `vision.register_model` — register a model under the plugin the host
//!   stamped as `owner`.
//! - `vision.infer` — run a registered engine-run model against one frame this
//!   engine published, named by its descriptor; the reply carries the batch.
//! - `vision.publish_detection` — publish a [`DetectionBatch`]. Used by
//!   plugin-side models and offloaded detection.
//! - `vision.designate_track` — lock a captured camera's tracker onto a box.
//! - `vision.request_detection` — take, renew or release a consumer's lease on
//!   detection for a captured camera ([`crate::engine`] demand gate).
//! - `vision.unregister_owner` — drop every model one plugin registered (sent
//!   by the host when it stops serving the plugin).
//! - `camera.control.set` — set exposure, gain or auto exposure on a camera
//!   this engine captures and answer the read-back
//!   ([`crate::camera_control`]).
//!
//! The `args` of the five plugin-facing requests are the shapes
//! [`ados_protocol::vision_rpc`] defines, decoded with its decoders so the SDK
//! that built them and this server cannot disagree.
//!
//! Each request gets one response envelope sharing the request's `request_id`;
//! an error sets the envelope `error` field, which the host surfaces to the
//! plugin verbatim.

use std::sync::Arc;

use ados_protocol::camera_control::CAMERA_CONTROL_METHOD;
use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::{methods, DetectionBatch, VISION_DETECTION_VERSION};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use ados_protocol::vision_rpc;
use anyhow::{anyhow, Result};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::engine::VisionEngine;

mod push;
use push::{spawn_detection_push, spawn_frame_push};

/// Fixed wait after a failed `accept()` before accepting again, with no cap.
const ACCEPT_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Bind `vision.sock` and serve clients until `cancel` is notified.
pub async fn serve(
    engine: Arc<VisionEngine>,
    socket_path: &str,
    cancel: ados_protocol::shutdown::Shutdown,
) -> Result<()> {
    // The shared helper owns the create-dir / remove-stale / bind / chmod
    // hygiene. 0o660 keeps the socket off the world while still reachable by the
    // agent group (the host connects as a peer; matches the plugin socket policy).
    let listener = ados_protocol::ipc::bind_command_socket(socket_path, 0o660)?;
    tracing::info!(path = %socket_path, "vision_sock_listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let engine = engine.clone();
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(engine, stream, cancel).await {
                                tracing::debug!(error = %e, "vision_sock_client_ended");
                            }
                        });
                    }
                    Err(e) => {
                        // Transient (fd pressure, an aborted connect): keep the
                        // socket up and accept again after a fixed wait, rather
                        // than leave every vision client refused until restart.
                        tracing::warn!(error = %e, "vision_sock_accept_failed");
                        tokio::select! {
                            _ = tokio::time::sleep(ACCEPT_RETRY_INTERVAL) => {}
                            _ = cancel.wait() => break,
                        }
                    }
                }
            }
            _ = cancel.wait() => break,
        }
    }
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// Serve one client connection. Reads request envelopes, dispatches each, and
/// writes the response. A `subscribe_frames` request additionally spawns a push
/// task that streams descriptors on the same connection for its lifetime.
async fn handle_client(
    engine: Arc<VisionEngine>,
    stream: UnixStream,
    cancel: ados_protocol::shutdown::Shutdown,
) -> Result<()> {
    let (mut read_half, write_half) = stream.into_split();
    // The writer is shared between the request-response path and the frame push
    // task, so both serialize their frames through one mutex.
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let mut frame_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut detection_task: Option<tokio::task::JoinHandle<()>> = None;
    // Held while this connection's detection subscription asked for
    // embeddings; dropped with the connection (or a re-subscribe without).
    let mut embedding_interest: Option<crate::engine::EmbeddingInterest> = None;

    loop {
        let mut header = [0u8; HEADER_SIZE];
        tokio::select! {
            r = read_half.read_exact(&mut header) => {
                if r.is_err() {
                    break;
                }
            }
            _ = cancel.wait() => break,
        }
        let len = match decode_len(header, PLUGIN_MAX_FRAME, true) {
            Ok(n) => n,
            Err(_) => break,
        };
        let mut body = vec![0u8; len];
        if read_half.read_exact(&mut body).await.is_err() {
            break;
        }
        let env = match Envelope::from_msgpack(&body) {
            Ok(e) => e,
            Err(_) => break,
        };

        if env.method == methods::SUBSCRIBE_FRAMES {
            // Acknowledge, then start (or restart) the descriptor push task.
            send_response(
                &writer,
                &env.request_id,
                ok_map(&[("subscribed", Value::Boolean(true))]),
                None,
            )
            .await?;
            if let Some(t) = frame_task.take() {
                t.abort();
            }
            frame_task = Some(spawn_frame_push(
                engine.clone(),
                writer.clone(),
                filter_camera(&env.args),
            ));
            continue;
        }

        if env.method == methods::SUBSCRIBE_DETECTIONS {
            // Acknowledge, then start (or restart) the detection-batch push task.
            let with_embeddings = embeddings_requested_in(&env.args);
            send_response(
                &writer,
                &env.request_id,
                ok_map(&[
                    ("subscribed", Value::Boolean(true)),
                    ("embeddings", Value::Boolean(with_embeddings)),
                ]),
                None,
            )
            .await?;
            if let Some(t) = detection_task.take() {
                t.abort();
            }
            embedding_interest = with_embeddings.then(|| engine.request_embeddings());
            detection_task = Some(spawn_detection_push(
                engine.clone(),
                writer.clone(),
                filter_camera(&env.args),
                with_embeddings,
            ));
            continue;
        }

        let (args, err) = dispatch(&engine, &env).await;
        send_response(&writer, &env.request_id, args, err).await?;
    }

    if let Some(t) = frame_task.take() {
        t.abort();
    }
    if let Some(t) = detection_task.take() {
        t.abort();
    }
    drop(embedding_interest);
    Ok(())
}

/// Run the request method and return `(response_args, optional_error)`.
async fn dispatch(engine: &Arc<VisionEngine>, env: &Envelope) -> (Value, Option<String>) {
    let result = match env.method.as_str() {
        m if m == methods::REGISTER_MODEL => handle_register(engine, &env.args).await,
        m if m == methods::INFER => handle_infer(engine, &env.args).await,
        m if m == methods::PUBLISH_DETECTION => handle_publish(engine, &env.args).await,
        m if m == methods::DESIGNATE_TRACK => handle_designate_track(engine, &env.args).await,
        m if m == methods::REQUEST_DETECTION => handle_request_detection(engine, &env.args).await,
        m if m == methods::LIST_MODELS => handle_list_models(engine).await,
        m if m == vision_rpc::UNREGISTER_OWNER => handle_unregister_owner(engine, &env.args).await,
        m if m == CAMERA_CONTROL_METHOD => crate::camera_control::handle(engine, &env.args).await,
        other => Err(anyhow!("unknown vision method {other}")),
    };
    match result {
        Ok(args) => (args, None),
        Err(e) => (Value::Map(Vec::new()), Some(e.to_string())),
    }
}

async fn handle_list_models(engine: &Arc<VisionEngine>) -> Result<Value> {
    // The engine's registered models, encoded as a msgpack Vec<ModelInfo> in a
    // binary field so the control-plane relay returns them unchanged. The
    // backend name + whether it actually runs inference ride alongside as
    // top-level fields, so a caller can ask "is a real backend loaded" (the
    // `/api/status` perception-tier honesty check) even when no model is
    // registered — `models` alone answers nothing in that case.
    let models = engine.list_models().await;
    let bytes = rmp_serde::to_vec_named(&models).map_err(|e| anyhow!("encode models: {e}"))?;
    let mut reply = vec![
        ("models", Value::Binary(bytes)),
        ("backend", Value::from(engine.backend_name())),
        (
            "backend_inference_capable",
            Value::Boolean(engine.is_inference_capable()),
        ),
        (
            "backend_state",
            Value::from(engine.backend_state().as_str()),
        ),
    ];
    if let Some(reason) = engine.backend_unavailable_reason() {
        reply.push(("backend_reason", Value::from(reason)));
    }
    Ok(ok_map(&reply))
}

/// Handle `vision.register_model`. The host stamps the calling plugin's id, and
/// the model lands under that plugin's namespace; the reply's `model_id` is the
/// registry id.
async fn handle_register(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let owner = vision_rpc::owner_of(args)?;
    let meta = vision_rpc::decode_register_model(args)?;
    let (model_id, exec, had_backend) = engine.register_owned_model(owner, meta).await?;
    Ok(ok_map(&[
        ("registered", Value::Boolean(true)),
        ("model_id", Value::from(model_id)),
        ("execution", Value::from(execution_str(exec))),
        ("backend_loaded", Value::Boolean(had_backend)),
    ]))
}

/// Handle `vision.unregister_owner`: drop every model one plugin registered.
async fn handle_unregister_owner(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let owner = vision_rpc::owner_of(args)?;
    let removed = engine.unregister_owner(owner).await;
    Ok(ok_map(&[("removed", Value::from(removed as u64))]))
}

/// Handle `vision.infer`: read the named frame out of the camera's ring (a torn
/// or recycled slot is an error, so the caller retries with a fresh descriptor)
/// and run the model on it for the plugin the host stamped. The reply is the
/// batch, not published: a plugin that wants it on the bus publishes it.
async fn handle_infer(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let owner = vision_rpc::owner_of(args)?;
    let req = vision_rpc::decode_infer(args)?;
    let desc = &req.frame;
    let mut pixels = Vec::with_capacity(desc.byte_len as usize);
    engine.read_frame_into(desc, &mut pixels).await?;
    let detections = engine
        .infer_for(
            owner,
            &req.model_id,
            &pixels,
            desc.width,
            desc.height,
            desc.format,
        )
        .await?;
    let batch = DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: req.model_id.clone(),
        camera_id: desc.camera_id.clone(),
        frame_id: desc.frame_id,
        ts_ms: (desc.ts_ns / 1_000_000) as i64,
        frame_width: desc.width,
        frame_height: desc.height,
        detections,
    };
    Ok(vision_rpc::infer_reply(&batch)?)
}

/// Handle `vision.publish_detection`. The decoder refuses a batch whose version
/// this build does not speak, so a mis-versioned batch is rejected loudly at
/// this plugin ingress rather than relayed to be mis-read downstream. The host
/// has already bound the batch to the publishing plugin (namespaced model id,
/// tracker fields cleared without designation rights). The camera id is not
/// checked against the engine's rings: an offloaded detector names the camera
/// it was fed, which need not be a camera this engine captures.
async fn handle_publish(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let batch = vision_rpc::decode_publish_detection(args)?;
    let reached = engine.publish_detection(batch);
    Ok(ok_map(&[("subscribers", Value::from(reached as u64))]))
}

/// Handle `vision.designate_track`: lock the named camera's tracker onto a
/// specific box (the operator's click-to-follow pick, or a plugin's),
/// overriding the auto-lock. Only a camera the engine captures has a tracker
/// to lock, so any other camera id is refused rather than growing the tracker
/// map.
async fn handle_designate_track(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let req = vision_rpc::decode_designate_track(args)?;
    if !engine.has_ring(&req.camera_id).await {
        return Err(anyhow!("no camera {} on this node", req.camera_id));
    }
    let track_id = engine.designate(&req.camera_id, &req.target).await;
    Ok(ok_map(&[
        ("designated", Value::Boolean(track_id.is_some())),
        ("track_id", track_id.map(Value::from).unwrap_or(Value::Nil)),
        ("camera_id", Value::from(req.camera_id)),
    ]))
}

/// Handle `vision.request_detection`: take, renew (`hold_ms > 0`) or release
/// (`hold_ms == 0`) the caller's detection lease on a captured camera. The
/// owner is the plugin the host stamped, or the name a control-plane caller
/// gave. Any camera the engine does not capture is refused, so the lease map
/// only ever names real cameras.
async fn handle_request_detection(engine: &Arc<VisionEngine>, args: &Value) -> Result<Value> {
    let req = vision_rpc::decode_request_detection(args)?;
    if !engine.has_ring(&req.camera_id).await {
        return Err(anyhow!("no camera {} on this node", req.camera_id));
    }
    let reply = engine.request_detection(
        &req.camera_id,
        &req.owner,
        req.hold_ms,
        ados_protocol::framebus::mono_ns() / 1_000_000,
    );
    Ok(vision_rpc::request_detection_reply(&reply))
}

/// Write a response envelope sharing `request_id`. An `error` sets the envelope
/// error field and an empty args map.
async fn send_response(
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    request_id: &str,
    args: Value,
    error: Option<String>,
) -> Result<()> {
    let env = Envelope {
        version: PROTOCOL_VERSION,
        kind: "response".to_string(),
        method: "response".to_string(),
        capability: String::new(),
        args: if error.is_some() {
            Value::Map(Vec::new())
        } else {
            args
        },
        request_id: request_id.to_string(),
        token: String::new(),
        error,
    };
    let frame = env
        .encode_frame()
        .map_err(|e| anyhow!("encode response: {e}"))?;
    let mut w = writer.lock().await;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// The optional `camera_id` filter on a `subscribe_frames` request.
fn filter_camera(args: &Value) -> Option<String> {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("camera_id"))
            .and_then(|(_, v)| v.as_str().map(str::to_owned)),
        _ => None,
    }
}

/// The optional `embeddings: true` flag on a `subscribe_detections` request:
/// the subscriber wants each detection's appearance embedding.
fn embeddings_requested_in(args: &Value) -> bool {
    match args {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some("embeddings"))
            .and_then(|(_, v)| v.as_bool())
            .unwrap_or(false),
        _ => false,
    }
}

/// Build a response args map from string-keyed pairs.
fn ok_map(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (Value::from(*k), v.clone()))
            .collect(),
    )
}

/// The wire string for a model execution kind.
fn execution_str(e: ados_protocol::framebus::ModelExecution) -> &'static str {
    use ados_protocol::framebus::ModelExecution::*;
    match e {
        EngineRun => "engine_run",
        PluginSide => "plugin_side",
    }
}

#[cfg(test)]
mod tests {
    use super::push::{deliver_detection, deliver_frame};
    use super::*;
    use crate::backend::MockBackend;
    use ados_protocol::framebus::{
        BoundingBox, Detection, FrameDescriptor, FrameFormat, ModelExecution, ModelKind,
        ModelMetadata,
    };

    fn engine() -> Arc<VisionEngine> {
        VisionEngine::new(Box::new(MockBackend), 4)
    }

    fn req_env(method: &str, args: Value) -> Envelope {
        Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".into(),
            method: method.into(),
            capability: String::new(),
            args,
            request_id: "rid-1".into(),
            token: String::new(),
            error: None,
        }
    }

    #[tokio::test]
    async fn register_model_dispatch_returns_registered() {
        let e = engine();
        let meta = ModelMetadata {
            id: "com.example.m".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 8,
            input_height: 8,
            input_format: FrameFormat::Rgb24,
            output_classes: vec!["x".into()],
            model_path: None,
            head: ados_protocol::framebus::DetectionHead::Yolo8,
        };
        // Unstamped (not through the host): refused.
        let bare = vision_rpc::register_model_args(&meta).unwrap();
        let (_resp, err) = dispatch(&e, &req_env(methods::REGISTER_MODEL, bare.clone())).await;
        assert!(err.unwrap().contains("owner"));

        let args = vision_rpc::with_owner(&bare, "com.example.p");
        let (resp, err) = dispatch(&e, &req_env(methods::REGISTER_MODEL, args)).await;
        assert!(err.is_none());
        // The response carries registered=true and the plugin-namespaced id.
        let map = as_map(&resp);
        assert_eq!(get(&map, "registered"), Some(Value::Boolean(true)));
        assert_eq!(
            get(&map, "model_id"),
            Some(Value::from("com.example.p/com.example.m"))
        );
        assert_eq!(get(&map, "execution"), Some(Value::from("engine_run")));
        assert_eq!(e.model_count().await, 1);

        // Dropping the plugin's models empties the registry.
        let (resp, err) = dispatch(
            &e,
            &req_env(
                vision_rpc::UNREGISTER_OWNER,
                vision_rpc::unregister_owner_args("com.example.p"),
            ),
        )
        .await;
        assert!(err.is_none());
        assert_eq!(get(&as_map(&resp), "removed"), Some(Value::from(1u64)));
        assert_eq!(e.model_count().await, 0);
    }

    #[tokio::test]
    async fn list_models_returns_the_registered_set() {
        use ados_protocol::framebus::{ModelExecution, ModelInfo, ModelKind};
        let e = engine();
        for (id, exec) in [
            ("b-model", ModelExecution::PluginSide),
            ("a-model", ModelExecution::EngineRun),
        ] {
            let meta = ModelMetadata {
                id: id.into(),
                kind: ModelKind::Detection,
                execution: exec,
                input_width: 8,
                input_height: 8,
                input_format: FrameFormat::Rgb24,
                output_classes: vec!["person".into()],
                model_path: None,
                head: ados_protocol::framebus::DetectionHead::Yolo8,
            };
            e.register_model(meta).await.unwrap();
        }

        let (resp, err) = dispatch(&e, &req_env(methods::LIST_MODELS, Value::Nil)).await;
        assert!(err.is_none());
        let map = as_map(&resp);
        let bytes = match get(&map, "models") {
            Some(Value::Binary(b)) => b,
            other => panic!("expected models binary, got {other:?}"),
        };
        let models: Vec<ModelInfo> = rmp_serde::from_slice(&bytes).unwrap();
        // Sorted by id, with execution + backend-loaded reported per model.
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "a-model");
        assert_eq!(models[0].execution, ModelExecution::EngineRun);
        assert_eq!(models[1].id, "b-model");
        assert_eq!(models[1].execution, ModelExecution::PluginSide);
        // The plugin-side model has no loaded backend.
        assert!(!models[1].backend_loaded);
        assert_eq!(models[0].output_classes, vec!["person".to_string()]);
    }

    /// Publish one 2x2 rgb24 frame on `uvc-0` and return its descriptor.
    async fn published_frame(e: &Arc<VisionEngine>) -> FrameDescriptor {
        e.publish_frame(
            "uvc-0",
            7,
            crate::ring::FrameTiming::host(1_700_000_000_000),
            2,
            2,
            FrameFormat::Rgb24,
            &[9u8; 12],
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn infer_runs_on_the_frame_a_descriptor_names() {
        let e = engine();
        let meta = ModelMetadata {
            id: "m".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 2,
            input_height: 2,
            input_format: FrameFormat::Rgb24,
            output_classes: vec![],
            model_path: None,
            head: ados_protocol::framebus::DetectionHead::Yolo8,
        };
        e.register_model(meta).await.unwrap();
        let desc = published_frame(&e).await;

        let args = vision_rpc::with_owner(&vision_rpc::infer_args("m", &desc).unwrap(), "p");
        let (resp, err) = dispatch(&e, &req_env(methods::INFER, args)).await;
        assert!(err.is_none(), "infer errored: {err:?}");
        let batch = vision_rpc::decode_infer_reply(&resp).unwrap();
        assert_eq!(batch.model_id, "m");
        assert_eq!(batch.camera_id, "uvc-0");
        assert_eq!(batch.frame_id, 7);
        assert_eq!((batch.frame_width, batch.frame_height), (2, 2));
        assert!(batch.detections.is_empty()); // mock backend
    }

    #[tokio::test]
    async fn infer_on_a_recycled_slot_or_unknown_model_errors() {
        let e = engine();
        let desc = published_frame(&e).await;
        let (_resp, err) = dispatch(
            &e,
            &req_env(
                methods::INFER,
                vision_rpc::with_owner(&vision_rpc::infer_args("nope", &desc).unwrap(), "p"),
            ),
        )
        .await;
        assert!(err.unwrap().contains("unknown model"));

        // A descriptor whose slot the ring has since recycled is refused, so the
        // caller retries with a fresh one instead of reading another frame.
        let mut stale = desc.clone();
        stale.seq += 1;
        let (_resp, err) = dispatch(
            &e,
            &req_env(
                methods::INFER,
                vision_rpc::with_owner(&vision_rpc::infer_args("nope", &stale).unwrap(), "p"),
            ),
        )
        .await;
        assert!(err.unwrap().contains("torn/stale"));
    }

    #[tokio::test]
    async fn publish_detection_dispatch_counts_subscribers() {
        let e = engine();
        let _rx = e.subscribe_detections();
        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "m".into(),
            camera_id: "c".into(),
            frame_id: 1,
            ts_ms: 0,
            frame_width: 640,
            frame_height: 480,
            detections: vec![],
        };
        let args = vision_rpc::publish_detection_args(&batch).unwrap();
        let (resp, err) = dispatch(&e, &req_env(methods::PUBLISH_DETECTION, args)).await;
        assert!(err.is_none());
        let map = as_map(&resp);
        assert_eq!(get(&map, "subscribers"), Some(Value::from(1u64)));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let e = engine();
        let (_resp, err) = dispatch(&e, &req_env("vision.bogus", Value::Map(vec![]))).await;
        assert!(err.is_some());
        assert!(err.unwrap().contains("unknown vision method"));
    }

    #[tokio::test]
    async fn camera_control_dispatch_answers_the_read_back() {
        use ados_protocol::camera_control::{reason, CameraControlRequest, CameraControlResult};
        let e = engine();
        e.camera_controls().register_not_v4l2("cam_fl");
        let req = CameraControlRequest {
            camera_id: "cam_fl".into(),
            exposure_us: Some(1_000),
            ..Default::default()
        };
        let (resp, err) = dispatch(&e, &req_env(CAMERA_CONTROL_METHOD, req.to_args())).await;
        assert_eq!(err, None);
        assert_eq!(
            CameraControlResult::from_value(&resp).unwrap(),
            CameraControlResult::refused(reason::NOT_V4L2)
        );
    }

    #[test]
    fn deliver_frame_carries_descriptor_binary() {
        let desc = FrameDescriptor {
            v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
            camera_id: "uvc-0".into(),
            frame_id: 1,
            ts_ns: 1,
            ts_sensor_ns: None,
            exposure_ns: None,
            trigger_seq: None,
            timebase: ados_protocol::framebus::TIMEBASE_HOST,
            width: 8,
            height: 8,
            format: FrameFormat::Rgb24,
            shm_name: "ados-vision-uvc-0".into(),
            slot: 0,
            seq: 3,
            byte_len: 192,
        };
        let frame = deliver_frame(&desc).unwrap();
        // Strip the length prefix and decode the envelope.
        let body = &frame[HEADER_SIZE..];
        let env = Envelope::from_msgpack(body).unwrap();
        assert_eq!(env.method, methods::DELIVER_FRAME);
        assert_eq!(env.kind, "event");
        // The descriptor round-trips out of the binary field.
        let bytes = match &env.args {
            Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some("descriptor"))
                .and_then(|(_, v)| match v {
                    Value::Binary(b) => Some(b.clone()),
                    _ => None,
                })
                .unwrap(),
            _ => panic!("args not a map"),
        };
        assert_eq!(FrameDescriptor::from_msgpack(&bytes).unwrap(), desc);
    }

    #[test]
    fn deliver_detection_carries_batch_binary() {
        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: "m".into(),
            camera_id: "uvc-0".into(),
            frame_id: 9,
            ts_ms: 5,
            frame_width: 640,
            frame_height: 480,
            detections: vec![Detection {
                bbox: Some(BoundingBox {
                    x: 1.0,
                    y: 2.0,
                    width: 3.0,
                    height: 4.0,
                }),
                class_label: "person".into(),
                confidence: 0.9,
                track_id: Some(7),
                assoc_confidence: None,
                lock_state: None,
                attributes: None,
                mask: None,
                keypoints: None,
                depth: None,
                world_pos: None,
            }],
        };
        let frame = deliver_detection(&batch).unwrap();
        let body = &frame[HEADER_SIZE..];
        let env = Envelope::from_msgpack(body).unwrap();
        assert_eq!(env.method, methods::DELIVER_DETECTION);
        assert_eq!(env.kind, "event");
        assert_eq!(env.capability, "vision.detection.subscribe");
        let bytes = match &env.args {
            Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_str() == Some("batch"))
                .and_then(|(_, v)| match v {
                    Value::Binary(b) => Some(b.clone()),
                    _ => None,
                })
                .unwrap(),
            _ => panic!("args not a map"),
        };
        assert_eq!(DetectionBatch::from_msgpack(&bytes).unwrap(), batch);
    }

    #[test]
    fn embeddings_flag_is_opt_in() {
        let yes = Value::Map(vec![(Value::from("embeddings"), Value::Boolean(true))]);
        let no = Value::Map(vec![(Value::from("embeddings"), Value::Boolean(false))]);
        assert!(embeddings_requested_in(&yes));
        assert!(!embeddings_requested_in(&no));
        assert!(!embeddings_requested_in(&Value::Map(vec![])));
        assert!(!embeddings_requested_in(&Value::Nil));
    }

    #[tokio::test]
    async fn list_models_reports_the_backend_state_and_reason() {
        let mock = engine();
        let (resp, err) = dispatch(&mock, &req_env(methods::LIST_MODELS, Value::Nil)).await;
        assert!(err.is_none());
        let map = as_map(&resp);
        assert_eq!(get(&map, "backend_state"), Some(Value::from("mock")));
        assert_eq!(get(&map, "backend_reason"), None);

        let missing = VisionEngine::new(
            Box::new(crate::backend::UnavailableBackend::new(
                "onnx",
                "not compiled in",
            )),
            4,
        );
        let (resp, err) = dispatch(&missing, &req_env(methods::LIST_MODELS, Value::Nil)).await;
        assert!(err.is_none());
        let map = as_map(&resp);
        assert_eq!(get(&map, "backend"), Some(Value::from("onnx")));
        assert_eq!(get(&map, "backend_state"), Some(Value::from("unavailable")));
        assert_eq!(
            get(&map, "backend_reason"),
            Some(Value::from("not compiled in"))
        );
        assert_eq!(
            get(&map, "backend_inference_capable"),
            Some(Value::Boolean(false))
        );
    }

    #[test]
    fn filter_camera_reads_optional_id() {
        let with = Value::Map(vec![(Value::from("camera_id"), Value::from("fpv"))]);
        assert_eq!(filter_camera(&with).as_deref(), Some("fpv"));
        assert_eq!(filter_camera(&Value::Map(vec![])), None);
    }

    // --- small helpers --------------------------------------------------
    #[tokio::test]
    async fn designate_track_dispatch_locks_a_camera() {
        let e = engine();
        e.publish_frame(
            "cam-0",
            1,
            crate::ring::FrameTiming::host(0),
            2,
            2,
            FrameFormat::Rgb24,
            &[0u8; 12],
        )
        .await
        .unwrap();
        // Mixed numeric encodings for the bbox fields exercise the coercion path.
        let args = Value::Map(vec![
            (Value::from("camera_id"), Value::from("cam-0")),
            (
                Value::from("bbox"),
                Value::Map(vec![
                    (Value::from("x"), Value::F64(10.0)),
                    (Value::from("y"), Value::Integer(20.into())),
                    (Value::from("width"), Value::F32(30.0)),
                    (Value::from("height"), Value::F64(40.0)),
                ]),
            ),
            (Value::from("class_label"), Value::from("person")),
        ]);
        let (resp, err) = dispatch(&e, &req_env(methods::DESIGNATE_TRACK, args)).await;
        assert!(err.is_none(), "designate dispatch errored: {err:?}");
        let map = as_map(&resp);
        assert_eq!(get(&map, "designated"), Some(Value::Boolean(true)));
        assert_eq!(get(&map, "camera_id"), Some(Value::from("cam-0")));
        assert!(
            matches!(get(&map, "track_id"), Some(Value::Integer(_))),
            "a track id was assigned"
        );
    }

    #[tokio::test]
    async fn designate_track_on_a_camera_without_a_ring_is_refused() {
        let e = engine();
        let args = vision_rpc::designate_track_args(
            "ghost-cam",
            &Detection {
                bbox: Some(BoundingBox {
                    x: 1.0,
                    y: 1.0,
                    width: 4.0,
                    height: 4.0,
                }),
                class_label: "person".into(),
                confidence: 0.9,
                track_id: None,
                assoc_confidence: None,
                lock_state: None,
                attributes: None,
                mask: None,
                keypoints: None,
                depth: None,
                world_pos: None,
            },
        )
        .unwrap();
        let (_resp, err) = dispatch(&e, &req_env(methods::DESIGNATE_TRACK, args)).await;
        assert!(err.unwrap().contains("no camera"));
        assert_eq!(e.current_track("ghost-cam").await, None);
    }

    #[tokio::test]
    async fn designate_track_missing_bbox_errors_softly() {
        let e = engine();
        let args = Value::Map(vec![(Value::from("camera_id"), Value::from("cam-0"))]);
        let (_resp, err) = dispatch(&e, &req_env(methods::DESIGNATE_TRACK, args)).await;
        assert!(err.is_some(), "a missing bbox is a soft error, not a panic");
    }

    #[tokio::test]
    async fn request_detection_leases_and_releases_a_captured_camera() {
        let e = engine();
        e.publish_frame(
            "belly",
            1,
            crate::ring::FrameTiming::host(0),
            2,
            2,
            FrameFormat::Rgb24,
            &[0u8; 12],
        )
        .await
        .unwrap();
        let now = ados_protocol::framebus::mono_ns() / 1_000_000;
        assert!(!e.detection_wanted("belly", now));

        let lease = |hold_ms: u64| {
            vision_rpc::with_owner(
                &vision_rpc::request_detection_args("belly", hold_ms),
                "com.example.follow",
            )
        };
        let (resp, err) = dispatch(&e, &req_env(methods::REQUEST_DETECTION, lease(6_000))).await;
        assert!(err.is_none(), "request_detection errored: {err:?}");
        let reply = vision_rpc::decode_request_detection_reply(&resp).unwrap();
        assert_eq!(reply.camera_id, "belly");
        assert!(reply.detecting);
        let until = reply.until_ms.expect("a lease end");
        assert!(until >= now + 6_000);
        assert!(e.detection_wanted("belly", now + 1));

        // Releasing turns it off again.
        let (resp, err) = dispatch(&e, &req_env(methods::REQUEST_DETECTION, lease(0))).await;
        assert!(err.is_none());
        let reply = vision_rpc::decode_request_detection_reply(&resp).unwrap();
        assert!(!reply.detecting);
        assert_eq!(reply.until_ms, None);
    }

    #[tokio::test]
    async fn request_detection_refuses_an_uncaptured_camera_and_a_bad_hold() {
        let e = engine();
        let ghost = vision_rpc::with_owner(
            &vision_rpc::request_detection_args("ghost-cam", 1_000),
            "operator",
        );
        let (_resp, err) = dispatch(&e, &req_env(methods::REQUEST_DETECTION, ghost)).await;
        assert!(err.unwrap().contains("no camera"));
        assert!(e.detection_status(&[], 0).cameras.is_empty());

        let too_long = vision_rpc::with_owner(
            &vision_rpc::request_detection_args("ghost-cam", vision_rpc::MAX_DETECTION_HOLD_MS + 1),
            "operator",
        );
        let (_resp, err) = dispatch(&e, &req_env(methods::REQUEST_DETECTION, too_long)).await;
        assert!(err.unwrap().contains("hold_ms"));
    }

    fn as_map(v: &Value) -> Vec<(Value, Value)> {
        match v {
            Value::Map(m) => m.clone(),
            _ => panic!("not a map"),
        }
    }
    fn get(map: &[(Value, Value)], key: &str) -> Option<Value> {
        map.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v.clone())
    }
}
