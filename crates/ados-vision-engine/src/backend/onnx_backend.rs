//! ONNX Runtime backend (opt-in `onnx` feature).

use super::*;

use ados_protocol::framebus::ModelKind;
use ados_vision::yolo;

/// Default detection thresholds, matching the Python sidecar's defaults.
const ONNX_CONF_THRESHOLD: f32 = 0.25;
const ONNX_NMS_IOU: f32 = 0.45;

/// ONNX Runtime backend. Loads `model_path` into an ORT session and runs it
/// on each frame. Built only under the `onnx` feature.
///
/// Every session runs on a fixed, small thread budget with spin-waiting off:
/// ORT's default (one intra-op thread per core, each spinning between nodes)
/// fights the flight stack for the CPU on a companion board and, under a CPU
/// quota, makes each inference several times slower than a single thread.
pub struct OnnxBackend {
    /// Intra-op threads for a detection model (`vision.detector.threads`).
    detector_threads: usize,
    /// Intra-op threads for a depth model (`vision.depth.threads`).
    depth_threads: usize,
}

struct OnnxModel {
    /// `Session::run` takes `&mut self`, but `LoadedModel::infer` is `&self`
    /// (the engine serializes inference on the accelerator lease anyway), so
    /// the session sits behind a mutex for interior mutability.
    session: Mutex<ort::session::Session>,
    /// The detector's input tensor, kept across frames and filled in place:
    /// several MB at 640x640, so no frame allocates one. Locked before the
    /// session.
    input: Mutex<Vec<f32>>,
    /// The stage times of the last detector pass.
    stages: Mutex<Option<StageTimes>>,
    /// The model's first input tensor name (e.g. `images`), captured at load.
    input_name: String,
    meta: ModelMetadata,
}

impl OnnxBackend {
    /// A backend whose detection sessions use `detector_threads` intra-op
    /// threads and whose depth sessions use `depth_threads` (each at least
    /// one). Every other model (the re-id embedder) uses one.
    pub fn new(detector_threads: usize, depth_threads: usize) -> Self {
        OnnxBackend {
            detector_threads: detector_threads.max(1),
            depth_threads: depth_threads.max(1),
        }
    }

    /// The intra-op thread count a session for `kind` gets.
    pub fn intra_threads_for(&self, kind: ModelKind) -> usize {
        match kind {
            ModelKind::Detection => self.detector_threads,
            ModelKind::Depth => self.depth_threads,
            _ => 1,
        }
    }
}

/// The process's ONNX Runtime environment, built on the first model load.
///
/// ONNX Runtime allows one environment per process and `ort` refuses a second
/// `build()` even after the first is dropped, so the environment is shared by
/// every backend and lives for the rest of the process. The runtime library
/// is loaded dynamically, so it is built here, not at start: a process that
/// never loads an ONNX model never needs the library.
fn onnx_environment() -> Result<ort::environment::Environment> {
    static ENV: Mutex<Option<ort::environment::Environment>> = Mutex::new(None);
    let mut env = ENV
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(e) = env.as_ref() {
        return Ok(e.clone());
    }
    let e = ort::init()
        .with_name("ados-vision")
        .build()
        .map_err(|e| anyhow!("init onnx runtime environment: {e}"))?;
    *env = Some(e.clone());
    Ok(e)
}

impl VisionBackend for OnnxBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        let path = meta
            .model_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("onnx model has no model_path"))?;
        let env = onnx_environment()?;
        let builder = ort::session::Session::builder(&env)?;
        let intra = self.intra_threads_for(meta.kind);
        // `BuilderResult` errors carry the builder back (large, and not `Send`
        // under every feature set), so each is flattened to a message at once.
        let threads = |e: ort::Error<ort::session::builder::SessionBuilder>| {
            anyhow!("configure onnx session threads: {e}")
        };
        #[allow(unused_mut)]
        let mut builder = builder
            .with_intra_threads(intra)
            .map_err(threads)?
            .with_inter_threads(1)
            .map_err(threads)?
            .with_intra_op_spinning(false)
            .map_err(threads)?
            .with_inter_op_spinning(false)
            .map_err(threads)?;
        // On the macOS workstation, register the CoreML execution provider so
        // inference runs on the Apple GPU/ANE instead of the CPU. Best-effort:
        // ORT falls back to CPU if CoreML cannot take a node. Only compiled in
        // under the `coreml` feature on macOS, so a Linux/drone build is
        // unaffected.
        #[cfg(all(feature = "coreml", target_os = "macos"))]
        {
            use ort::ep::CoreML;
            // `with_execution_providers` returns `ort::Error<SessionBuilder>`,
            // which is not `Send` under the coreml feature (it carries a
            // non-Send OperatorDomain), so `?`-into-anyhow would not compile.
            // Format the error to a string and wrap it fresh instead.
            builder = builder
                .with_execution_providers([CoreML::default().build()])
                .map_err(|e| anyhow!("register CoreML execution provider: {e}"))?;
        }
        let session = builder.commit_from_file(path)?;
        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or_else(|| anyhow::anyhow!("onnx model has no inputs"))?;
        // A self-describing file states its own head, classes and input size.
        let mut meta = meta.clone();
        if let Ok(md) = session.metadata() {
            super::model_props::apply_model_props(&mut meta, |k| md.custom(k));
        }
        Ok(Box::new(OnnxModel {
            session: Mutex::new(session),
            input: Mutex::new(Vec::new()),
            stages: Mutex::new(None),
            input_name,
            meta,
        }))
    }
    fn name(&self) -> &str {
        "onnx"
    }
}

impl OnnxModel {
    /// One detector pass: `window` of the frame (`None`: all of it) resized
    /// into the reused input tensor, run, and decoded to boxes in an
    /// `out_w` x `out_h` image of that window.
    #[allow(clippy::too_many_arguments)]
    fn detect(
        &self,
        frame: &[u8],
        w: u32,
        h: u32,
        f: FrameFormat,
        window: Option<&BoundingBox>,
        out_w: u32,
        out_h: u32,
    ) -> Result<Vec<Detection>> {
        if f != FrameFormat::Rgb24 {
            return Err(anyhow!(
                "onnx backend requires rgb24 frames, got {f:?}; \
                 feed an rgb24-converted frame"
            ));
        }
        let iw = self.meta.input_width;
        let ih = self.meta.input_height;
        let t0 = Instant::now();
        let mut chw = self
            .input
            .lock()
            .map_err(|_| anyhow!("onnx input lock poisoned"))?;
        if !yolo::preprocess_rgb24_nchw_into(frame, w, h, window, iw, ih, &mut chw) {
            return Err(anyhow!("onnx preprocess failed (frame too small?)"));
        }
        let t1 = Instant::now();
        let input = ort::value::TensorRef::from_array_view((
            [1usize, 3, ih as usize, iw as usize],
            chw.as_slice(),
        ))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("onnx session lock poisoned"))?;
        let outputs = session.run(ort::inputs![self.input_name.as_str() => input])?;
        let t2 = Instant::now();
        let value = outputs
            .iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| anyhow!("onnx model produced no output"))?;
        let (shape, data) = value.try_extract_tensor::<f32>()?;
        let (rows, cols) = last_two_dims(shape);

        let params = yolo::DecodeParams {
            head: self.meta.head,
            labels: &self.meta.output_classes,
            input_w: iw,
            input_h: ih,
            frame_w: out_w,
            frame_h: out_h,
            conf_threshold: ONNX_CONF_THRESHOLD,
            nms_iou: ONNX_NMS_IOU,
        };
        let found = yolo::decode(data, rows, cols, &params);
        let ms = |a: Instant, b: Instant| b.duration_since(a).as_secs_f32() * 1000.0;
        if let Ok(mut s) = self.stages.lock() {
            *s = Some(StageTimes {
                preprocess_ms: ms(t0, t1),
                run_ms: ms(t1, t2),
                decode_ms: ms(t2, Instant::now()),
            });
        }
        Ok(found)
    }
}

impl LoadedModel for OnnxModel {
    fn infer(&self, frame: &[u8], w: u32, h: u32, f: FrameFormat) -> Result<Vec<Detection>> {
        self.detect(frame, w, h, f, None, w, h)
    }

    fn infer_window(
        &self,
        frame: &[u8],
        w: u32,
        h: u32,
        f: FrameFormat,
        window: &BoundingBox,
        input_w: u32,
        input_h: u32,
    ) -> Result<Vec<Detection>> {
        self.detect(frame, w, h, f, Some(window), input_w, input_h)
    }

    fn last_stages(&self) -> Option<StageTimes> {
        self.stages.lock().ok().and_then(|s| *s)
    }

    fn embed(&self, crop: &[u8], w: u32, h: u32, f: FrameFormat) -> Result<Option<Vec<f32>>> {
        if f != FrameFormat::Rgb24 {
            return Err(anyhow!("onnx embed requires rgb24 crops, got {f:?}"));
        }
        let iw = self.meta.input_width;
        let ih = self.meta.input_height;
        if w != iw || h != ih {
            return Err(anyhow!(
                "onnx embed crop {w}x{h} does not match model input {iw}x{ih}"
            ));
        }
        // The re-id ONNX expects ImageNet-normalized input (applied here, the
        // same way the detector path divides by 255 outside the model).
        let chw = ados_vision::reid::preprocess_osnet_nchw(crop, w, h)
            .ok_or_else(|| anyhow!("onnx embed preprocess failed (crop too small)"))?;
        let input = ort::value::Tensor::from_array(([1usize, 3, ih as usize, iw as usize], chw))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("onnx session lock poisoned"))?;
        let outputs = session.run(ort::inputs![self.input_name.as_str() => input])?;
        let value = outputs
            .iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| anyhow!("onnx embed produced no output"))?;
        let (_, data) = value.try_extract_tensor::<f32>()?;
        // The engine L2-normalizes uniformly across backends (the trait
        // contract), so the raw model output is returned here.
        Ok(Some(data.to_vec()))
    }

    fn infer_dense(&self, frame: &[u8], w: u32, h: u32, f: FrameFormat) -> Result<DenseMap> {
        if f != FrameFormat::Rgb24 {
            return Err(anyhow!("onnx depth requires rgb24 frames, got {f:?}"));
        }
        let iw = self.meta.input_width;
        let ih = self.meta.input_height;
        let chw = super::preprocess_depth_nchw(frame, w, h, iw, ih)
            .ok_or_else(|| anyhow!("onnx depth preprocess failed (frame too small?)"))?;
        let input = ort::value::Tensor::from_array(([1usize, 3, ih as usize, iw as usize], chw))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow!("onnx session lock poisoned"))?;
        let outputs = session.run(ort::inputs![self.input_name.as_str() => input])?;
        let value = outputs
            .iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| anyhow!("onnx depth model produced no output"))?;
        let (shape, data) = value.try_extract_tensor::<f32>()?;
        // `[1, H, W]` or `[1, 1, H, W]`: the map is the last two axes.
        let (rows, cols) = last_two_dims(shape);
        DenseMap::new(cols as u32, rows as u32, data.to_vec())
    }

    fn resolved_metadata(&self) -> Option<&ModelMetadata> {
        Some(&self.meta)
    }
}

/// The last two dimensions of an output shape (the feature/anchor axes after
/// the batch dim). A 1-D output is read as a single row; an empty shape is
/// `(0, 0)` so the decode yields nothing.
fn last_two_dims(shape: &[i64]) -> (usize, usize) {
    let dims: Vec<usize> = shape.iter().map(|&d| d.max(0) as usize).collect();
    match dims.len() {
        0 => (0, 0),
        1 => (1, dims[0]),
        n => (dims[n - 2], dims[n - 1]),
    }
}

#[cfg(test)]
mod thread_tests {
    use super::*;

    #[test]
    fn detection_and_depth_sessions_take_their_threads_and_every_other_model_one() {
        let b = OnnxBackend::new(2, 3);
        assert_eq!(b.intra_threads_for(ModelKind::Detection), 2);
        assert_eq!(b.intra_threads_for(ModelKind::Depth), 3);
        assert_eq!(b.intra_threads_for(ModelKind::Tracking), 1);
        // Zero is never a valid ORT budget (it would mean "one per core").
        let zero = OnnxBackend::new(0, 0);
        assert_eq!(zero.intra_threads_for(ModelKind::Detection), 1);
        assert_eq!(zero.intra_threads_for(ModelKind::Depth), 1);
    }
}
