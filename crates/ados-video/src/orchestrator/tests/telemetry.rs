//! Pipeline telemetry: only real metrics, and none while idle.

use super::*;

// --- no placeholder telemetry -------------------------------------------

#[tokio::test]
async fn emit_telemetry_emits_only_the_two_real_metrics() {
    // The streaming-copy path has no source for queue depth / dropped
    // frames, so emit_telemetry must NOT emit them as constant-0.0
    // placeholders. The emitter's enqueue counter is the capture seam: each
    // emit_metric call enqueues exactly one frame, so the count proves the
    // emitted set. After the fix only two metrics (bitrate + framerate) flow
    // per pass, not four.
    use ados_protocol::logd::emitter::IngestEmitter;

    let mut o = test_orch();
    o.state = PipelineState::Running;
    // A bogus socket path means no live daemon; frames still enqueue into
    // the channel buffer so the enqueue count is exact for a single pass
    // (the channel capacity dwarfs two sends, so nothing drops here).
    let emitter = IngestEmitter::with_socket("ados-video-test", "/nonexistent/ados/ingest.sock");
    let stats = emitter.stats();
    o.emit_telemetry(&emitter);
    assert_eq!(
        stats.enqueued() + stats.dropped(),
        2,
        "exactly two metrics must be produced (bitrate + framerate); the two placeholders are gone"
    );
}

#[tokio::test]
async fn emit_telemetry_silent_when_not_running() {
    // An idle / errored pipeline has no encoder, so no telemetry is emitted.
    use ados_protocol::logd::emitter::IngestEmitter;
    let o = test_orch();
    assert_ne!(o.state, PipelineState::Running);
    let emitter = IngestEmitter::with_socket("ados-video-test", "/nonexistent/ados/ingest.sock");
    let stats = emitter.stats();
    o.emit_telemetry(&emitter);
    assert_eq!(stats.enqueued() + stats.dropped(), 0);
}
