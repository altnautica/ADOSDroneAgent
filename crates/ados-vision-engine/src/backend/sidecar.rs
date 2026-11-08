//! The accelerator sidecar IPC client.

use super::*;

mod wire;
use wire::*;
#[cfg(test)]
pub(super) use wire::{map_get, mv};

/// Cap on a single framed message, matching the sidecar's `MAX_FRAME_BYTES`.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// How long one liveness observation of a sidecar is trusted before it is
/// re-taken. Short enough that a sidecar that died reads as not-capable within
/// a single `/api/status` poll, long enough that a status flood does not open a
/// socket per request.
const SIDECAR_LIVENESS_TTL: Duration = Duration::from_secs(3);

/// An IPC client to the Python accelerator sidecar. The sidecar owns the vendor
/// NPU runtime (RKNN on Rockchip, HailoRT on a Pi AI HAT); this backend forwards
/// `load_model` and `infer` requests to it and decodes the detection reply.
///
/// The socket path is resolved once at construction. A load or infer call that
/// cannot reach the sidecar returns an error, which the engine treats as a
/// degraded model (it falls back to the mock model for that registration) so a
/// missing sidecar never crashes the engine.
pub struct SidecarBackend {
    socket_path: String,
    name: String,
    /// Observed liveness of the sidecar behind `socket_path`, shared with every
    /// model this backend loads so a real exchange updates the same state the
    /// status surface reads.
    health: Arc<SidecarHealth>,
}

/// Observed liveness of one sidecar socket.
///
/// [`SidecarBackend::load`] cannot tell whether the sidecar is up — the
/// handshake is deliberately deferred to the first `infer` so the registry
/// records the model either way. Without this, capability fell through to the
/// trait default `true`, so `/api/status` advertised `perception_tier: "local"`
/// for a socket nothing was listening on: a permanently empty detection stream
/// reported as a working accelerator.
#[derive(Debug, Default)]
struct SidecarHealth {
    /// The most recent observation: when it was taken, and whether the sidecar
    /// answered. `None` until the first attempt.
    last: Mutex<Option<(Instant, bool)>>,
}

impl SidecarHealth {
    /// Record the outcome of a real exchange or a connect probe.
    fn record(&self, reachable: bool) {
        *self.last.lock().expect("sidecar health lock") = Some((Instant::now(), reachable));
    }

    /// The last observation while it is still inside [`SIDECAR_LIVENESS_TTL`].
    fn fresh(&self) -> Option<bool> {
        match *self.last.lock().expect("sidecar health lock") {
            Some((at, reachable)) if at.elapsed() < SIDECAR_LIVENESS_TTL => Some(reachable),
            _ => None,
        }
    }
}

/// Is anything listening on the sidecar socket right now?
///
/// A connect, not a `Path::exists`: a sidecar killed with SIGKILL leaves its
/// socket inode behind, so an existence check reports the dead case as healthy,
/// which is the exact fabrication this guards against. The connection is closed
/// immediately; the sidecar's accept loop treats it as a client that hung up.
fn probe_sidecar(socket_path: &str) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

impl SidecarBackend {
    /// `name` is the accelerator the sidecar owns behind this socket — "rknn"
    /// (Rockchip) or "hailo" (a Hailo-8 on a Pi AI HAT). The wire
    /// protocol is identical; only the socket and the reported name differ,
    /// because the vendor runtime lives in the Python sidecar, not here.
    pub fn new(socket_path: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            name: name.into(),
            health: Arc::new(SidecarHealth::default()),
        }
    }

    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }
}

struct SidecarModel {
    socket_path: String,
    model_id: String,
    model_path: String,
    input_w: u32,
    input_h: u32,
    input_format: String,
    class_labels: Vec<String>,
    /// The output-head layout the sidecar must decode with (`yolov8` | `yolov5`).
    head: String,
    /// Whether `load_model` has been sent to the sidecar this session. The
    /// sidecar keeps loaded models across connections, so we send the load
    /// handshake once (lazily, on the first infer) and re-send it only if the
    /// sidecar reports the model is no longer loaded (it was restarted).
    loaded: Mutex<bool>,
    /// The backend's liveness state. Every exchange this model makes updates
    /// it, so the capability the status surface reads is the outcome of real
    /// traffic rather than a guess.
    health: Arc<SidecarHealth>,
}

impl VisionBackend for SidecarBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        // Capture the load parameters; the sidecar handshake is deferred to the
        // first infer so the registry can record the model even when the sidecar
        // is not up yet (a missing model_path degrades to an unreachable sidecar
        // at infer time, which the engine treats as a degraded model).
        Ok(Box::new(SidecarModel {
            socket_path: self.socket_path.clone(),
            model_id: meta.id.clone(),
            model_path: meta.model_path.clone().unwrap_or_default(),
            input_w: meta.input_width,
            input_h: meta.input_height,
            input_format: fmt_str(meta.input_format).to_string(),
            class_labels: meta.output_classes.clone(),
            head: head_str(meta.head).to_string(),
            loaded: Mutex::new(false),
            health: Arc::clone(&self.health),
        }))
    }
    fn name(&self) -> &str {
        &self.name
    }

    /// Capable only while the sidecar is actually answering.
    ///
    /// The vendor runtime is in another process, so "the backend was selected"
    /// says nothing about whether inference can run: the sidecar may never have
    /// started, may have died, or may have been killed with its socket inode
    /// left behind. A recent real exchange is the strongest evidence and is
    /// used when there is one; otherwise a connect probe answers, and the
    /// result is cached for [`SIDECAR_LIVENESS_TTL`] so a status poll never
    /// turns into a connect per request.
    fn is_inference_capable(&self) -> bool {
        if let Some(reachable) = self.health.fresh() {
            return reachable;
        }
        let reachable = probe_sidecar(&self.socket_path);
        self.health.record(reachable);
        reachable
    }
}

impl SidecarModel {
    /// One round trip that also records what it learned about the sidecar.
    ///
    /// Every exchange this model makes is evidence of liveness, and it is
    /// stronger evidence than a connect probe, so the capability the status
    /// surface reads tracks real traffic: an infer that reaches the sidecar
    /// marks it reachable, and the first one that cannot marks it dead.
    fn exchange(&self, req: &rmpv::Value) -> Result<rmpv::Value> {
        let out = round_trip(&self.socket_path, req);
        self.health.record(out.is_ok());
        out
    }

    /// Send `load_model` to the sidecar once. Idempotent: a no-op after the
    /// first success until [`mark_unloaded`] resets it.
    fn ensure_loaded(&self) -> Result<()> {
        let mut loaded = self.loaded.lock().expect("rknn load lock");
        if *loaded {
            return Ok(());
        }
        let req = load_request(
            &self.model_id,
            &self.model_path,
            self.input_w,
            self.input_h,
            &self.input_format,
            &self.class_labels,
            &self.head,
        );
        let resp = self.exchange(&req)?;
        check_ok(&resp).context("sidecar load_model")?;
        *loaded = true;
        Ok(())
    }

    fn mark_unloaded(&self) {
        *self.loaded.lock().expect("rknn load lock") = false;
    }
}

impl LoadedModel for SidecarModel {
    fn infer(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<Vec<Detection>> {
        let fmt = fmt_str(format);
        self.ensure_loaded()?;
        let req = frame_request("infer", &self.model_id, frame, width, height, fmt);
        let resp = self.exchange(&req)?;
        match decode_detections(&resp) {
            Ok(dets) => Ok(dets),
            Err(e) if is_not_loaded(&e) => {
                // The sidecar restarted and dropped the model: reload once and retry.
                self.mark_unloaded();
                self.ensure_loaded()?;
                let resp2 = self.exchange(&req)?;
                decode_detections(&resp2)
            }
            Err(e) => Err(e),
        }
    }

    fn embed(
        &self,
        crop: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<Option<Vec<f32>>> {
        let fmt = fmt_str(format);
        self.ensure_loaded()?;
        let req = embed_request(&self.model_id, crop, width, height, fmt);
        let resp = self.exchange(&req)?;
        match decode_embedding(&resp) {
            Ok(emb) => Ok(Some(emb)),
            Err(e) if is_not_loaded(&e) => {
                // The sidecar restarted and dropped the model: reload + retry once.
                self.mark_unloaded();
                self.ensure_loaded()?;
                let resp2 =
                    self.exchange(&embed_request(&self.model_id, crop, width, height, fmt))?;
                decode_embedding(&resp2).map(Some)
            }
            Err(e) => Err(e),
        }
    }

    fn infer_dense(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<DenseMap> {
        let fmt = fmt_str(format);
        self.ensure_loaded()?;
        let req = frame_request("dense", &self.model_id, frame, width, height, fmt);
        let resp = self.exchange(&req)?;
        match decode_dense(&resp) {
            Ok(map) => Ok(map),
            Err(e) if is_not_loaded(&e) => {
                // The sidecar restarted and dropped the model: reload + retry once.
                self.mark_unloaded();
                self.ensure_loaded()?;
                decode_dense(&self.exchange(&req)?)
            }
            Err(e) => Err(e),
        }
    }
}
