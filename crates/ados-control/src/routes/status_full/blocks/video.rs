//! Video block: the multi-process branch (no in-process pipeline). The verdict
//! itself is ados-video's [`ados_video::status_verdict`], shared with the cloud
//! heartbeat so both surfaces report the same state.

use ados_video::status_verdict::{
    build_video_block_with, read_pipeline_outcome, resolve_mediamtx_ready,
};
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

/// Build the consolidated-status `video` block for the multi-process case:
/// resolve the live mediamtx readiness once (the real loopback probes), read the
/// pipeline sidecar and the per-leg streams, and hand them to the shared pure
/// verdict. The recording block is always empty (no in-process recorder on this
/// daemon).
pub(crate) async fn build_video_block(
    resolved_profile: &str,
    wfb_status: &Option<Map<String, Value>>,
) -> Value {
    build_video_block_with(
        resolved_profile,
        wfb_status,
        resolve_mediamtx_ready().await,
        read_video_streams(),
        read_pipeline_outcome(),
    )
}

/// Read the video-streams sidecar (`/run/ados/video-streams.json`, written by
/// ados-video on pipeline start) and resolve each leg to the agent's own
/// same-origin WHEP + HLS proxy paths, so the status surface advertises every leg
/// stream switcher. Best-effort: any read/parse failure yields an empty list.
pub(crate) fn read_video_streams() -> Vec<Value> {
    let Ok(body) = std::fs::read("/run/ados/video-streams.json") else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_slice::<Value>(&body) else {
        return Vec::new();
    };
    let Some(streams) = doc.get("streams").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    streams
        .iter()
        .filter_map(|leg| {
            let id = leg.get("id")?.as_str()?;
            Some(json!({
                "id": id,
                "role": leg.get("role").and_then(|r| r.as_str()).unwrap_or(""),
                "codec": leg.get("codec").and_then(|c| c.as_str()).unwrap_or(""),
                "whepUrl": format!("/whep?camera={id}"),
                "hls": format!("/hls/{id}/index.m3u8"),
                // Per-leg liveness (null when the agent has not sampled it yet).
                "live": leg.get("live").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}
