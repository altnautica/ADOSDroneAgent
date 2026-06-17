//! `ados-vision`: the agent-side vision engine host.
//!
//! The engine owns the camera-frame fast path that plugins are too small and
//! too sandboxed to own. For every engine-owned camera it captures (or taps)
//! frames, normalizes them to a downscaled [`ados_protocol::framebus::FrameFormat`],
//! and publishes them into a per-camera shared-memory ring; consumers map the
//! same ring and read the slot a small descriptor names. It also keeps the
//! registry of inference models, serializes inference on the shared accelerator
//! behind a lease arbiter, and fans detections back out.
//!
//! Plugins never touch the engine directly. They speak the plugin RPC envelope
//! to the host, the host gates each call on a vision capability and proxies it
//! to this engine over `/run/ados/vision.sock` — the same 4-byte big-endian
//! length-prefixed msgpack framing the MAVLink socket uses.
//!
//! Modules:
//! - [`ring`] — the single-writer frame ring writer over a `/dev/shm` mapping
//!   (with an in-memory fallback so it builds and tests off Linux).
//! - [`source`] — the frame sources: a tap reader off the video pipeline and a
//!   direct V4L2/CSI capture, behind one [`source::FrameSource`] trait.
//! - [`backend`] — the inference backends: a mock, an opt-in ONNX Runtime one,
//!   and an IPC client to the accelerator sidecar.
//! - [`engine`] — the per-camera rings + sources, the model registry, the
//!   accelerator lease arbiter, and the detection publisher.
//! - [`visionsock`] — the `/run/ados/vision.sock` request/response server.
//! - [`detection_bus`] — the `/run/ados/vision-detections.sock` broadcast that
//!   re-publishes every detection batch as length-prefixed msgpack so the API
//!   process can forward it to the browser over a WebSocket.
//! - [`tracker`] — a single-object visual tracker that turns the noisy per-frame
//!   detection stream into one stable identity by filling `track_id` on the
//!   canonical detection type (the lock primitive follow-me / framing /
//!   target-lock consumers read).
//! - [`config`] — the `vision:` block of the agent config.

pub mod backend;
pub mod config;
pub mod detection_bus;
pub mod engine;
pub mod ring;
pub mod source;
pub mod tracker;
pub mod visionsock;
