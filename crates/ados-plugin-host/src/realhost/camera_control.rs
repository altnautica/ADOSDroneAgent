//! `camera.control.set`: relay a plugin's exposure and gain request to the
//! vision engine, which owns the camera's device and answers the read-back.
//!
//! The request is decoded and re-encoded, so only the declared fields reach
//! the engine, and the engine's reply is decoded before it is relayed, so a
//! plugin always gets the [`CameraControlResult`] shape.

use ados_protocol::camera_control::{
    CameraControlRequest, CameraControlResult, CAMERA_CONTROL_METHOD,
};

use super::*;

impl RealHost {
    pub(super) async fn relay_camera_control(&self, args: &Value) -> Result<HostResult, HostError> {
        let Some(client) = self.vision.as_ref() else {
            return Ok(not_implemented(CAMERA_CONTROL_METHOD));
        };
        let req = CameraControlRequest::from_args(args).map_err(HostError::Rpc)?;
        let reply = client
            .camera_control_set(&req.to_args())
            .await
            .map_err(|e| HostError::Rpc(e.0))?;
        let result = CameraControlResult::from_value(&reply).map_err(HostError::Rpc)?;
        Ok(result.to_value())
    }
}

#[cfg(test)]
#[path = "camera_control_tests.rs"]
mod tests;
