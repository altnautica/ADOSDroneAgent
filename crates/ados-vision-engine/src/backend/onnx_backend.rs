//! ONNX Runtime backend (opt-in `onnx` feature).

use super::*;

use ados_vision::yolo;

/// Default detection thresholds, matching the Python sidecar's defaults.
const ONNX_CONF_THRESHOLD: f32 = 0.25;
const ONNX_NMS_IOU: f32 = 0.45;

/// ONNX Runtime backend. Loads `model_path` into an ORT session and runs it
/// on each frame. Built only under the `onnx` feature.
pub struct OnnxBackend;

struct OnnxModel {
    /// `Session::run` takes `&mut self`, but `LoadedModel::infer` is `&self`
    /// (the engine serializes inference on the accelerator lease anyway), so
    /// the session sits behind a mutex for interior mutability.
    session: Mutex<ort::session::Session>,
    /// The model's first input tensor name (e.g. `images`), captured at load.
    input_name: String,
    meta: ModelMetadata,
}

#[allow(clippy::new_without_default)]
impl OnnxBackend {
    pub fn new() -> Self {
        OnnxBackend
    }
}

impl VisionBackend for OnnxBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        let path = meta
            .model_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("onnx model has no model_path"))?;
        let mut builder = ort::session::Session::builder()?;
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
        Ok(Box::new(OnnxModel {
            session: Mutex::new(session),
            input_name,
            meta: meta.clone(),
        }))
    }
    fn name(&self) -> &str {
        "onnx"
    }
}

impl LoadedModel for OnnxModel {
    fn infer(&self, frame: &[u8], w: u32, h: u32, f: FrameFormat) -> Result<Vec<Detection>> {
        if f != FrameFormat::Rgb24 {
            return Err(anyhow!(
                "onnx backend requires rgb24 frames, got {f:?}; \
                 feed an rgb24-converted frame"
            ));
        }
        let iw = self.meta.input_width;
        let ih = self.meta.input_height;
        let chw = yolo::preprocess_rgb24_nchw(frame, w, h, iw, ih)
            .ok_or_else(|| anyhow!("onnx preprocess failed (frame too small?)"))?;

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
            .ok_or_else(|| anyhow!("onnx model produced no output"))?;
        let (shape, data) = value.try_extract_tensor::<f32>()?;
        let (rows, cols) = last_two_dims(shape);

        let params = yolo::DecodeParams {
            head: self.meta.head,
            labels: &self.meta.output_classes,
            input_w: iw,
            input_h: ih,
            frame_w: w,
            frame_h: h,
            conf_threshold: ONNX_CONF_THRESHOLD,
            nms_iou: ONNX_NMS_IOU,
        };
        Ok(yolo::decode(data, rows, cols, &params))
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
