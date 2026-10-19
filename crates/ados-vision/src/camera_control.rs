//! Routes `camera.control.set` to the capture thread that owns the camera.
//!
//! Each source the engine opens registers here under its camera id: a V4L2
//! capture gets a queue its capture thread drains between buffers, applying
//! each request on the device handle it streams from; a simulated RTSP rig or
//! the video pipeline's tap is recorded as having no controls. A reopened
//! source replaces its camera's route.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ados_protocol::camera_control::{reason, CameraControlRequest, CameraControlResult};
use anyhow::{anyhow, Result};
use rmpv::Value;
use tokio::sync::{mpsc, oneshot};

use crate::engine::VisionEngine;
use crate::source::{ControlCall, ControlQueue};

/// Requests one camera may have waiting for its capture thread.
const QUEUE_DEPTH: usize = 4;
/// How long a request waits for the capture thread. The thread applies
/// requests between buffers and a dequeue blocks for at most two seconds.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);

enum Route {
    Capture(mpsc::Sender<ControlCall>),
    NotV4l2,
}

/// The control route of every camera the engine runs. Cheap to clone; every
/// clone shares one table.
#[derive(Clone, Default)]
pub struct CameraControls {
    routes: Arc<Mutex<HashMap<String, Route>>>,
}

impl CameraControls {
    fn insert(&self, camera_id: &str, route: Route) {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(camera_id.to_string(), route);
    }

    /// Route `camera_id` to a V4L2 capture; its capture thread drains the
    /// returned queue.
    pub fn register_capture(&self, camera_id: &str) -> ControlQueue {
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        self.insert(camera_id, Route::Capture(tx));
        rx
    }

    /// Record `camera_id` as a source without device controls.
    pub fn register_other(&self, camera_id: &str) {
        self.insert(camera_id, Route::NotV4l2);
    }

    /// Apply `req` on its camera and return the read-back.
    pub async fn set(&self, req: &CameraControlRequest) -> CameraControlResult {
        let sender = {
            let routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
            match routes.get(&req.camera_id) {
                None => return CameraControlResult::refused(reason::UNKNOWN_CAMERA),
                Some(Route::NotV4l2) => return CameraControlResult::refused(reason::NOT_V4L2),
                Some(Route::Capture(tx)) => tx.clone(),
            }
        };
        let (reply, answer) = oneshot::channel();
        let call = ControlCall {
            request: req.clone(),
            reply,
        };
        match sender.try_send(call) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return CameraControlResult::refused(reason::BUSY)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return CameraControlResult::refused(reason::NOT_STREAMING)
            }
        }
        match tokio::time::timeout(CONTROL_TIMEOUT, answer).await {
            Ok(Ok(result)) => result,
            // The capture thread ended, or is stalled on a device that no
            // longer delivers buffers.
            Ok(Err(_)) | Err(_) => CameraControlResult::refused(reason::NOT_STREAMING),
        }
    }
}

/// `camera.control.set` on `vision.sock`: decode, apply, answer the result.
pub async fn handle(engine: &VisionEngine, args: &Value) -> Result<Value> {
    let req = CameraControlRequest::from_args(args).map_err(|e| anyhow!(e))?;
    Ok(engine.camera_controls().set(&req).await.to_value())
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::source::controls::{apply, ControlDevice, ControlInfo};

    /// `V4L2_CID_EXPOSURE_ABSOLUTE` and `V4L2_CID_EXPOSURE_AUTO`.
    const EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
    const EXPOSURE_AUTO: u32 = 0x009a_0901;

    /// One integer control per id, every write kept as is.
    #[derive(Default)]
    struct Device(HashMap<u32, i32>);

    impl ControlDevice for Device {
        fn query(&mut self, id: u32) -> Option<ControlInfo> {
            self.0.contains_key(&id).then_some(ControlInfo {
                minimum: 0,
                maximum: 10_000,
                step: 1,
            })
        }
        fn set(&mut self, id: u32, value: i32) -> io::Result<()> {
            self.0.insert(id, value);
            Ok(())
        }
        fn get(&mut self, id: u32) -> io::Result<i32> {
            self.0
                .get(&id)
                .copied()
                .ok_or_else(|| io::ErrorKind::InvalidInput.into())
        }
    }

    /// A stand-in capture thread answering `queue` until every sender is gone.
    fn capture_thread(mut queue: ControlQueue, mut device: Device) {
        std::thread::spawn(move || {
            while let Some(call) = queue.blocking_recv() {
                let _ = call.reply.send(apply(&mut device, &call.request));
            }
        });
    }

    fn req(camera_id: &str) -> CameraControlRequest {
        CameraControlRequest {
            camera_id: camera_id.into(),
            exposure_us: Some(1_500),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_capture_route_applies_on_the_owning_thread() {
        let controls = CameraControls::default();
        let device = Device(HashMap::from([(EXPOSURE_ABSOLUTE, 10), (EXPOSURE_AUTO, 3)]));
        capture_thread(controls.register_capture("cam_down"), device);
        let r = controls.set(&req("cam_down")).await;
        assert!(r.applied, "{r:?}");
        assert_eq!(r.exposure_us, Some(1_500));
    }

    #[tokio::test]
    async fn sources_without_a_device_and_unknown_cameras_are_refused() {
        let controls = CameraControls::default();
        controls.register_other("cam_fl");
        assert_eq!(
            controls.set(&req("cam_fl")).await,
            CameraControlResult::refused(reason::NOT_V4L2)
        );
        assert_eq!(
            controls.set(&req("nope")).await,
            CameraControlResult::refused(reason::UNKNOWN_CAMERA)
        );
    }

    #[tokio::test]
    async fn a_capture_that_ended_or_is_backed_up_is_refused() {
        let controls = CameraControls::default();
        drop(controls.register_capture("cam"));
        assert_eq!(
            controls.set(&req("cam")).await.reason.as_deref(),
            Some(reason::NOT_STREAMING)
        );

        // A capture thread that never drains: the queue fills up.
        let held = controls.register_capture("cam");
        let waiting: Vec<_> = (0..QUEUE_DEPTH)
            .map(|_| {
                let c = controls.clone();
                tokio::spawn(async move { c.set(&req("cam")).await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            controls.set(&req("cam")).await.reason.as_deref(),
            Some(reason::BUSY)
        );
        drop(held);
        for w in waiting {
            assert_eq!(
                w.await.unwrap().reason.as_deref(),
                Some(reason::NOT_STREAMING)
            );
        }
    }

    #[tokio::test]
    async fn a_reopened_source_replaces_the_route() {
        let controls = CameraControls::default();
        drop(controls.register_capture("cam"));
        controls.register_other("cam");
        assert_eq!(
            controls.set(&req("cam")).await.reason.as_deref(),
            Some(reason::NOT_V4L2)
        );
    }

    #[tokio::test]
    async fn the_rpc_handler_decodes_and_answers_the_wire_shape() {
        let engine = VisionEngine::new(Box::new(crate::backend::MockBackend), 4);
        engine.camera_controls().register_other("cam_fl");
        let reply = handle(&engine, &req("cam_fl").to_args()).await.unwrap();
        assert_eq!(
            CameraControlResult::from_value(&reply).unwrap(),
            CameraControlResult::refused(reason::NOT_V4L2)
        );
        assert!(handle(&engine, &Value::Map(vec![])).await.is_err());
    }
}
