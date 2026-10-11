use super::*;
use crate::process_manager::NullManager;

/// A process manager that records the units it was asked to restart, so a
/// dispatch test can assert the video service was (or was not) restarted
/// without touching systemd.
struct RecordingPm {
    restarted: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ProcessManager for RecordingPm {
    async fn start(&self, _unit: &str) -> bool {
        true
    }
    async fn stop(&self, _unit: &str) -> bool {
        true
    }
    async fn restart(&self, unit: &str) -> bool {
        self.restarted.lock().unwrap().push(unit.to_string());
        true
    }
    async fn try_restart(&self, unit: &str) -> bool {
        self.restarted.lock().unwrap().push(unit.to_string());
        true
    }
    async fn reset_failed(&self, _unit: &str) {}
    async fn is_active(&self, _unit: &str) -> Option<bool> {
        Some(true)
    }
    async fn mask(&self, _unit: &str) {}
    async fn unmask(&self, _unit: &str) {}
}

#[tokio::test]
async fn unknown_op_is_rejected_without_a_restart() {
    let pm = RecordingPm {
        restarted: std::sync::Mutex::new(Vec::new()),
    };
    let resp = dispatch(br#"{"op":"gpio.output.set"}"#, &pm).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "E_UNKNOWN_OP");
    assert!(pm.restarted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_json_is_rejected() {
    let resp = dispatch(b"not json", &NullManager).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "E_PARSE");
}

#[tokio::test]
async fn video_cameras_set_op_is_routed_not_unknown() {
    // The operator-surface op is accepted (reaches validation, an empty list
    // is an E_ARGS not E_UNKNOWN_OP), proving it is routed alongside
    // video.source.set without touching the real config.
    let resp = dispatch(
        br#"{"op":"video.cameras.set","owner":"operator","cameras":[]}"#,
        &NullManager,
    )
    .await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "E_ARGS");
}

#[tokio::test]
async fn non_array_and_empty_and_incomplete_lists_are_rejected() {
    for body in [
        br#"{"op":"video.source.set","cameras":"main"}"#.as_slice(),
        br#"{"op":"video.source.set","cameras":[]}"#.as_slice(),
        br#"{"op":"video.source.set","cameras":[{"id":"main"}]}"#.as_slice(),
        // A leg id with a mediamtx-unsafe character (a space / slash).
        br#"{"op":"video.source.set","cameras":[{"id":"bad id","source":"rtsp://x/y"}]}"#
            .as_slice(),
        br#"{"op":"video.source.set","cameras":[{"id":"a/b","source":"rtsp://x/y"}]}"#
            .as_slice(),
        // Duplicate leg ids would collapse in the mediamtx path map.
        br#"{"op":"video.source.set","cameras":[{"id":"main","source":"rtsp://x/1"},{"id":"main","source":"rtsp://x/2"}]}"#
            .as_slice(),
        // A non-primary leg named "main" collides with the primary, which
        // always resolves to the reserved "main" path — the submitted ids
        // ("eo" vs "main") differ so the uniqueness check above misses it.
        br#"{"op":"video.source.set","cameras":[{"id":"eo","role":"primary","source":"rtsp://x/zoom"},{"id":"main","role":"ir","source":"rtsp://x/ir"}]}"#
            .as_slice(),
    ] {
        let resp = dispatch(body, &NullManager).await;
        assert_eq!(resp["ok"], false, "body should reject: {body:?}");
        assert_eq!(resp["error"], "E_ARGS");
    }
}

#[tokio::test]
async fn a_plugin_source_write_cannot_claim_the_operator_owner() {
    let pm = RecordingPm {
        restarted: std::sync::Mutex::new(Vec::new()),
    };
    let resp = dispatch(
        br#"{"op":"video.source.set","owner":"operator","cameras":[{"id":"belly","source":"rtsp://example.com/x"}]}"#,
        &pm,
    )
    .await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "E_ARGS");
    assert!(resp["reason"]
        .as_str()
        .unwrap()
        .starts_with("owner_conflict"));
    assert!(pm.restarted.lock().unwrap().is_empty());
}

// The valid-list path (persist + restart) writes the real /etc/ados config
// and is exercised on-rig: the config merge is unit-tested in bind::keys and
// the restart is a single call after a successful write. A dispatch test of
// it here would touch the host's real config path, so it stays bench-gated.
