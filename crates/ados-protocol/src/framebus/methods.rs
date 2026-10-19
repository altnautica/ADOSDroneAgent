/// Subscribe to `vision.frame` descriptors. Gated on `vision.frame.read`.
/// The host then delivers descriptors as `vision.deliver` events.
pub const SUBSCRIBE_FRAMES: &str = "vision.subscribe_frames";
/// Register an inference model. Gated on `vision.model.register`.
pub const REGISTER_MODEL: &str = "vision.register_model";
/// Run a registered model against one frame on the shared backend, returning
/// detections. Gated on `vision.model.register`.
pub const INFER: &str = "vision.infer";
/// Publish a detection batch on `vision.detection`. Gated on
/// `vision.detection.publish`.
pub const PUBLISH_DETECTION: &str = "vision.publish_detection";

/// Subscribe to published `DetectionBatch`es. Gated on
/// `vision.detection.subscribe`. The host then delivers each batch as a
/// `vision.deliver_detection` event (mirrors `subscribe_frames`).
pub const SUBSCRIBE_DETECTIONS: &str = "vision.subscribe_detections";

/// Event method the host uses to push a detection batch to a subscriber.
pub const DELIVER_DETECTION: &str = "vision.deliver_detection";

/// Designate the engine's single-object follow target: lock the camera's
/// tracker onto a specific box (the operator's click-to-follow pick).
/// Gated on `vision.track.designate`; the control plane's click-to-follow
/// route sends it too.
pub const DESIGNATE_TRACK: &str = "vision.designate_track";

/// List the engine's registered models — a read-back of the model registry
/// so the GCS vision hub can show every model loaded on the drone (not just
/// the one publishing detections). No args; the reply carries the models as
/// a msgpack-encoded `Vec<ModelInfo>` in a binary field.
pub const LIST_MODELS: &str = "vision.list_models";

/// Read the CALLING plugin's own resolved model-delivery status. No args; the
/// reply is `{models: [ {state, model_id, runtime, path, reason}, ... ]}` —
/// the `model_status` the host resolved from the plugin's declared model
/// refs, so the plugin can find where its delivered model was cached. This is
/// the model-delivery last mile: unlike `LIST_MODELS` (engine registry) this
/// is per-plugin resolution keyed by `model_id`.
pub const READ_MODEL: &str = "vision.read_model";

/// Event method the host uses to push a frame descriptor to a subscriber
/// (mirrors `mavlink.deliver`).
pub const DELIVER_FRAME: &str = "vision.deliver";
