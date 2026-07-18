//! The drone-side offload return bridge: gate + convert + republish.
//!
//! Detections returned from a compute node re-enter the drone here exactly as a
//! local detector's would, so nothing downstream (the host overlay, Follow-Me,
//! the gimbal) knows or cares where the compute happened. Two pieces:
//!
//! - [`OffloadReturnBridge`] is the pure logic: it drives the `ados-offload`
//!   [`OffloadSession`] safety gate (freshness + lock), converts a normalized
//!   [`OffloadDetectionBatch`] into the pixel-space [`DetectionBatch`] the vision
//!   bus speaks, and stamps the designated track's lock state from the gate. No
//!   I/O, so the safety behaviour is unit-tested with no sockets.
//! - [`VisionSockPublisher`] owns the `/run/ados/vision.sock` connection and
//!   publishes each converted batch (`vision.publish_detection`) onto the drone's
//!   local `vision.detection` bus.
//!
//! Safety invariant (the whole reason `ados-offload` exists): an offloaded
//! detection is actionable only while it is fresh AND the link is up. The moment
//! freshness or the link fails, the session lock drops to `Lost` and the
//! consuming behaviour stops — a returned box is proof of a past frame, not of a
//! live target. The bridge never extrapolates a stale box and never auto-
//! re-acquires a dropped lock.

use ados_offload::{OffloadMode, OffloadSession, SessionStatus};
use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::framebus::{
    self, methods, BoundingBox, Detection as BusDetection, DetectionBatch, VISION_DETECTION_VERSION,
};
use ados_protocol::offload::OffloadDetectionBatch;
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Drives the offload safety gate and converts returned batches into the vision
/// bus's `DetectionBatch`. Pure logic (no I/O); the publisher does the socket
/// write.
pub struct OffloadReturnBridge {
    session: OffloadSession,
    /// The track the operator designated to follow (a click-to-track pick), so
    /// the gate's lock state is stamped onto that track's box on the bus.
    designated_track: Option<u64>,
    /// The model id stamped on the published batch (identifies the offload source
    /// on the bus / in the overlay).
    model_id: String,
}

impl OffloadReturnBridge {
    /// A bridge for `mode` with the target-age freshness budget (`pose_budget_ms`
    /// matters only for `Full`/`SlamOnly`). `model_id` labels the published batch.
    pub fn new(
        mode: OffloadMode,
        target_budget_ms: i64,
        pose_budget_ms: i64,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            session: OffloadSession::new(mode, target_budget_ms, pose_budget_ms),
            designated_track: None,
            model_id: model_id.into(),
        }
    }

    /// Designate a track to follow (the operator's click-to-track), acquiring the
    /// safety lock. The only way out of a `Lost` lock.
    pub fn designate(&mut self, track_id: u64) {
        self.designated_track = Some(track_id);
        self.session.designate();
    }

    /// Release the lock (back to idle).
    pub fn drop_lock(&mut self) {
        self.designated_track = None;
        self.session.drop_lock();
    }

    /// The link changed (a drop trips the gate to `Lost`).
    pub fn set_link(&mut self, up: bool) {
        self.session.set_link(up);
    }

    /// Advance the gate one cycle without a new detection — the periodic tick a
    /// consumer runs so a stream that stops arriving (or a dropped link) trips the
    /// lock to `Lost` on the local clock, even though no batch arrived to convert.
    pub fn tick(&mut self, now_ms: i64) -> SessionStatus {
        self.session.tick(now_ms)
    }

    /// Ingest a returned batch: record its freshness, advance the gate, convert to
    /// the pixel-space vision-bus batch, and stamp the designated track's box with
    /// the gate's lock state. Returns the batch to publish + the safety snapshot a
    /// behaviour acts on (`commanding` is true only when locked AND fresh).
    pub fn ingest(
        &mut self,
        batch: &OffloadDetectionBatch,
        now_ms: i64,
    ) -> (DetectionBatch, SessionStatus) {
        self.session.on_detection(now_ms);
        let status = self.session.tick(now_ms);
        let lock = map_lock(status.lock);
        let detections = batch
            .detections
            .iter()
            .map(|d| self.to_bus_detection(d, batch.width, batch.height, lock))
            .collect();
        let db = DetectionBatch {
            v: VISION_DETECTION_VERSION,
            model_id: self.model_id.clone(),
            camera_id: batch.camera_id.clone(),
            // The per-session frame sequence is the frame identity on the bus.
            frame_id: batch.seq,
            ts_ms: batch.ts_ms,
            // The pixel space the boxes above were denormalized into — the
            // offload session's frame size — so the GCS overlay scales to it
            // instead of guessing (fixes offloaded boxes landing off the target).
            frame_width: batch.width,
            frame_height: batch.height,
            detections,
        };
        (db, status)
    }

    /// Denormalize one offload detection into the pixel-space bus detection,
    /// stamping the gate's lock state onto the designated track (only). A
    /// non-designated box carries no lock state (the source is stateless).
    fn to_bus_detection(
        &self,
        d: &ados_protocol::offload::Detection,
        width: u32,
        height: u32,
        lock: Option<framebus::LockState>,
    ) -> BusDetection {
        let fw = width.max(1) as f32;
        let fh = height.max(1) as f32;
        let is_designated = self.designated_track.is_some() && d.track_id == self.designated_track;
        BusDetection {
            bbox: Some(BoundingBox {
                x: d.bbox[0] * fw,
                y: d.bbox[1] * fh,
                width: d.bbox[2] * fw,
                height: d.bbox[3] * fh,
            }),
            class_label: d.class.clone(),
            confidence: d.confidence,
            track_id: d.track_id,
            assoc_confidence: None,
            lock_state: if is_designated { lock } else { None },
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        }
    }
}

/// Map the offload session's lock state onto the vision bus's per-detection lock
/// state. `Unlocked` (idle, nothing designated) carries no lock state.
fn map_lock(lock: ados_offload::LockState) -> Option<framebus::LockState> {
    match lock {
        ados_offload::LockState::Locked => Some(framebus::LockState::Locked),
        ados_offload::LockState::Lost => Some(framebus::LockState::Lost),
        ados_offload::LockState::Unlocked => None,
    }
}

/// Publishes converted batches onto the drone's local `vision.detection` bus over
/// `/run/ados/vision.sock` (`vision.publish_detection`). Holds one connection and
/// reconnects on error; each publish reads its response so the vision server's
/// per-request reply is drained (unread replies would backpressure it).
pub struct VisionSockPublisher {
    path: String,
    stream: Option<UnixStream>,
    req_counter: u64,
}

impl VisionSockPublisher {
    /// A publisher targeting the vision request socket (`/run/ados/vision.sock`).
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            stream: None,
            req_counter: 0,
        }
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.stream.is_none() {
            let s = UnixStream::connect(&self.path)
                .await
                .map_err(|e| anyhow!("connect {}: {e}", self.path))?;
            self.stream = Some(s);
        }
        Ok(())
    }

    /// Publish one batch. Returns the subscriber count the vision engine reported
    /// (0 = no on-bus subscribers, not an error). A socket error drops the
    /// connection so the next call reconnects.
    pub async fn publish(&mut self, batch: &DetectionBatch) -> Result<usize> {
        self.ensure_connected().await?;
        match self.publish_inner(batch).await {
            Ok(n) => Ok(n),
            Err(e) => {
                // Drop the stream so the next publish reconnects rather than
                // reusing a half-written / dead connection.
                self.stream = None;
                Err(e)
            }
        }
    }

    async fn publish_inner(&mut self, batch: &DetectionBatch) -> Result<usize> {
        self.req_counter = self.req_counter.wrapping_add(1);
        let args = rmpv::ext::to_value(batch).map_err(|e| anyhow!("encode batch args: {e}"))?;
        let env = Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".into(),
            method: methods::PUBLISH_DETECTION.into(),
            capability: String::new(),
            args,
            request_id: format!("offload-pub-{}", self.req_counter),
            token: String::new(),
            error: None,
        };
        let frame = env
            .encode_frame()
            .map_err(|e| anyhow!("encode publish envelope: {e}"))?;

        let stream = self.stream.as_mut().expect("connected above");
        stream.write_all(&frame).await?;

        // Read the one response envelope the server writes per request.
        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await?;
        let len = decode_len(header, PLUGIN_MAX_FRAME, true)
            .map_err(|e| anyhow!("bad response header: {e}"))?;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
        let resp = Envelope::from_msgpack(&body).map_err(|e| anyhow!("decode response: {e}"))?;
        if let Some(err) = resp.error {
            return Err(anyhow!("vision publish rejected: {err}"));
        }
        // The reply args carry `{subscribers: N}`; read it best-effort.
        let subs = resp
            .args
            .as_map()
            .and_then(|m| {
                m.iter().find_map(|(k, v)| {
                    (k.as_str() == Some("subscribers"))
                        .then(|| v.as_u64())
                        .flatten()
                })
            })
            .unwrap_or(0);
        Ok(subs as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::offload::Detection as OffDet;

    fn off_batch(track: Option<u64>) -> OffloadDetectionBatch {
        OffloadDetectionBatch::new(
            "s1",
            "front",
            3,
            1000,
            1280,
            720,
            vec![OffDet {
                bbox: [0.25, 0.25, 0.5, 0.5],
                class: "person".into(),
                confidence: 0.8,
                track_id: track,
            }],
        )
    }

    #[test]
    fn ingest_denormalizes_the_box_to_the_frame_size() {
        let mut bridge = OffloadReturnBridge::new(OffloadMode::VisionOnly, 500, 500, "offload");
        let (db, _status) = bridge.ingest(&off_batch(Some(1)), 1000);
        assert_eq!(db.v, VISION_DETECTION_VERSION);
        assert_eq!(db.camera_id, "front");
        assert_eq!(db.frame_id, 3);
        // The batch carries the frame size the boxes were denormalized into, so
        // the GCS overlay scales to it instead of guessing 640x480 (offloaded
        // boxes would otherwise land off the target).
        assert_eq!(db.frame_width, 1280);
        assert_eq!(db.frame_height, 720);
        assert_eq!(db.detections.len(), 1);
        let b = db.detections[0].bbox.as_ref().unwrap();
        // 0.25*1280 / 0.25*720 / 0.5*1280 / 0.5*720
        assert_eq!(b.x, 320.0);
        assert_eq!(b.y, 180.0);
        assert_eq!(b.width, 640.0);
        assert_eq!(b.height, 360.0);
        assert_eq!(db.detections[0].class_label, "person");
    }

    #[test]
    fn a_designated_fresh_track_locks_and_commands() {
        let mut bridge = OffloadReturnBridge::new(OffloadMode::VisionOnly, 500, 500, "offload");
        bridge.designate(1);
        let (db, status) = bridge.ingest(&off_batch(Some(1)), 1000);
        // The gate is locked + fresh -> commanding.
        assert!(status.commanding);
        // ...and the designated box carries the Locked lock state.
        assert_eq!(
            db.detections[0].lock_state,
            Some(framebus::LockState::Locked)
        );
    }

    #[test]
    fn a_non_designated_box_carries_no_lock_state() {
        let mut bridge = OffloadReturnBridge::new(OffloadMode::VisionOnly, 500, 500, "offload");
        bridge.designate(99); // designate a DIFFERENT track
        let (db, _status) = bridge.ingest(&off_batch(Some(1)), 1000);
        assert_eq!(db.detections[0].lock_state, None);
    }

    #[test]
    fn a_link_drop_trips_the_lock_to_lost_and_stops_commanding() {
        let mut bridge = OffloadReturnBridge::new(OffloadMode::VisionOnly, 500, 500, "offload");
        bridge.designate(1);
        assert!(bridge.ingest(&off_batch(Some(1)), 1000).1.commanding);
        // The link drops; a periodic tick trips the gate to Lost.
        bridge.set_link(false);
        let status = bridge.tick(1050);
        assert_eq!(status.lock, ados_offload::LockState::Lost);
        assert!(!status.commanding);
    }

    #[test]
    fn a_stale_stream_stops_commanding_and_never_auto_re_acquires() {
        let mut bridge = OffloadReturnBridge::new(OffloadMode::VisionOnly, 500, 500, "offload");
        bridge.designate(1);
        assert!(bridge.ingest(&off_batch(Some(1)), 1000).1.commanding);
        // No new batch; the local clock passes the freshness budget -> Lost.
        assert_eq!(bridge.tick(2000).lock, ados_offload::LockState::Lost);
        // Fresh batches resume, but the lock must NOT come back on its own.
        let (db, status) = bridge.ingest(&off_batch(Some(1)), 2100);
        assert!(!status.commanding, "no auto-re-acquire after a stale drop");
        assert_eq!(db.detections[0].lock_state, Some(framebus::LockState::Lost));
        // Only a re-designate re-acquires.
        bridge.designate(1);
        assert!(bridge.ingest(&off_batch(Some(1)), 2150).1.commanding);
    }
}
