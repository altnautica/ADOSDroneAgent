//! The video pipeline, WebRTC playback, the camera roster, the attention
//! profile, node-local recording and the native vision routes.

use axum::extract::DefaultBodyLimit;
use axum::routing::post;
use http::Method;

use crate::routes::table::Routes;
use crate::routes::{
    camera_config, gs_recording, video, video_config_write, video_profile, video_snapshot,
    video_status, vision, vision_detector, vision_upload, whep,
};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // The composite status (cameras, mediamtx `main` readiness, tool
    // dependencies, WHEP/HLS URLs), the discovered-camera enumeration, a
    // one-shot still, glass-to-glass latency, and the encoder/radio config read
    // plus the link-tuning write.
    r.get("/api/video", video_status::get_video_status)
        .get("/api/video/cameras", video_status::get_video_camera_list)
        .get("/api/video/snapshot", video_snapshot::get_video_snapshot)
        .get("/api/video/latency", video::get_video_latency)
        .get("/api/video/config", video::get_video_config)
        .post("/api/video/config", video_config_write::post_video_config)
        // The reconciled per-node camera roster and the operator write that
        // persists the leg list (the supervisor's merge-by-owner persist).
        .get("/api/video/roster", camera_config::get_video_cameras)
        .put("/api/video/roster", camera_config::put_video_cameras)
        // Attention profile: hero (full video) or thumbnail. GET reads the
        // state, POST sets the base or a hero lease, DELETE releases a lease.
        .get("/api/video/profile", video_profile::get_video_profile)
        .post("/api/video/profile", video_profile::post_video_profile)
        .delete("/api/video/profile", video_profile::delete_video_profile)
        // Node-local recording on any profile, over the recorder the
        // ground-station routes drive.
        .post(
            "/api/video/record/start",
            gs_recording::post_video_record_start,
        )
        .post(
            "/api/video/record/stop",
            gs_recording::post_video_record_stop,
        )
        // WebRTC playback: the WHEP offer and the session-resource teardown and
        // ICE restart, relayed to mediamtx on loopback.
        .post("/whep", whep::post_whep)
        .delete("/whep/{session_id}", whep::delete_whep)
        .patch("/whep/{session_id}", whep::patch_whep)
        // Vision: operator click-to-follow, the registered-model read-back,
        // the capability read, and the detector selection. The rest of
        // `/api/vision` stays on the residual.
        .post("/api/vision/designate", vision::designate)
        .get("/api/vision/status", vision::engine_status)
        .get("/api/vision/capabilities", vision::engine_capabilities)
        .put("/api/vision/detector", vision_detector::put_detector)
        .delete("/api/vision/detector", vision_detector::delete_detector)
        // Custom-model upload. The default 2 MB body cap is lifted for this one
        // route: a real detector file exceeds it. The handler streams the field
        // to disk chunk by chunk, so the model is never buffered in RAM.
        .method(
            "/api/vision/models/upload",
            Method::POST,
            post(vision_upload::upload_model).layer(DefaultBodyLimit::disable()),
        )
}
