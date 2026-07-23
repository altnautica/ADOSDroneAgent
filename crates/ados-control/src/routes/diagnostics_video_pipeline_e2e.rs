//! Bytes-every-hop video-pipeline integration test — the hardware-free half.
//!
//! This drives the SHIPPED per-hop video verifier (`GET /api/diag/video`) end to
//! end against a controllable fixture of the hop counters, proving the harness both
//! (a) reports every hop `flowing` while each canonical counter ADVANCES over the
//! sampling window, and (b) attributes a real stall to the exact hop where the data
//! stopped (`video_dies_at`). It reuses the shipped hop model + verdict vocabulary
//! (`flowing` / `stalled` / `no_upstream` / `unknown` + `video_dies_at`) — it does
//! not invent a parallel verdict.
//!
//! Reliable-diagnostics discipline (the pipeline runbook): a hop is judged by a
//! cumulative/rate counter's DELTA across two reads a window apart, on the CANONICAL
//! objects — the wfb-stats sidecar (`packets_received` / `fanout_forwarded` /
//! `tx_bytes_per_s` / `link_diag`) and the mediamtx `main` ingest counter — never a
//! single snapshot and never process-liveness. So each scenario builds a REAL pair
//! of window samples: the wfb-stats sidecar is a real JSON file read off disk (the
//! sidecar canonical object, driven through the real `read_run_sidecar_at` read and
//! the real accessors), rewritten between the two reads to model a counter that
//! advances or freezes; the mediamtx ingest counter is injected per read (a
//! controllable mock of that hop counter — its live HTTP read is separate plumbing).
//! Both drive the real `drone_hops` / `gs_hops` / `resolve_hops` verdict + the real
//! `build_video_diagnostics` JSON assembly.
//!
//! What the full Atlas sim-bench VM run adds on top of this (the bench step, an
//! internal harness at the monorepo root): a synthetic camera feeding the REAL agent
//! video pipeline (ffmpeg → mediamtx → wfb tee → wfb_tx) on an OrbStack Linux VM, so
//! the mediamtx `bytesReceived` and the wfb-stats sidecar are produced by the live
//! services rather than injected/written by the test — end-to-end proof that the
//! same verifier this test exercises reads the real running pipeline correctly.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::{build_video_diagnostics, VideoSample};

/// Write a wfb-stats sidecar (`<dir>/wfb-stats.json`) with the given object body,
/// the file the ground/drone wfb service publishes and the verifier samples.
fn write_wfb_sidecar(dir: &Path, body: Value) {
    fs::write(
        dir.join("wfb-stats.json"),
        serde_json::to_string(&body).unwrap(),
    )
    .unwrap();
}

/// The per-hop verdicts, in flow order.
fn verdicts(body: &Value) -> Vec<String> {
    body["hops"]
        .as_array()
        .expect("hops is an array")
        .iter()
        .map(|h| h["verdict"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// The hop names, in flow order.
fn hop_names(body: &Value) -> Vec<String> {
    body["hops"]
        .as_array()
        .expect("hops is an array")
        .iter()
        .map(|h| h["name"].as_str().unwrap_or("?").to_string())
        .collect()
}

// --- ground station (video SINK): RF → decode → fan-out → served WHEP ----------

#[test]
fn gs_every_hop_flows_while_the_counters_advance_over_the_window() {
    // A real sidecar on disk; the decoded-packets rate is positive and the
    // fan-out + mediamtx ingest counters ADVANCE between the two window reads.
    let dir = tempfile::tempdir().unwrap();

    write_wfb_sidecar(
        dir.path(),
        json!({ "packets_received": 550, "fanout_forwarded": 10_000 }),
    );
    let s0 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);

    // The window elapses; the agent rewrites the sidecar with an advanced fan-out
    // total and mediamtx has ingested more bytes.
    write_wfb_sidecar(
        dir.path(),
        json!({ "packets_received": 550, "fanout_forwarded": 11_000 }),
    );
    let s1 = VideoSample::from_run_dir(dir.path(), Some(2_000), true);

    let body = build_video_diagnostics("ground-station", &Some("direct".into()), &s0, &s1);

    // The response envelope.
    assert_eq!(body["profile"], json!("ground-station"));
    assert_eq!(body["role"], json!("direct"));
    assert_eq!(body["canonical_path"], json!("main"));
    assert_eq!(body["window_s"], json!(2.0));

    assert_eq!(
        hop_names(&body),
        vec!["rf_to_decode", "decode_to_fanout", "fanout_to_served"]
    );
    // Every counter advanced → every hop flows → nothing died.
    assert_eq!(verdicts(&body), vec!["flowing", "flowing", "flowing"]);
    assert_eq!(body["video_dies_at"], Value::Null);
}

#[test]
fn gs_a_frozen_fanout_counter_is_located_at_decode_to_fanout() {
    // Decoding stays healthy (packets/s positive) but the cumulative fan-out total
    // is FLAT across the window — the datagrams stop leaving the decoder. With the
    // fan-out starved, mediamtx-gs ingests nothing new either (its counter is flat).
    let dir = tempfile::tempdir().unwrap();

    write_wfb_sidecar(
        dir.path(),
        json!({ "packets_received": 550, "fanout_forwarded": 10_000 }),
    );
    let s0 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);

    write_wfb_sidecar(
        dir.path(),
        json!({ "packets_received": 550, "fanout_forwarded": 10_000 }),
    );
    let s1 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);

    let body = build_video_diagnostics("ground-station", &Some("direct".into()), &s0, &s1);

    // Decode flows; the fan-out is where it dies; the served hop is merely starved.
    assert_eq!(verdicts(&body), vec!["flowing", "stalled", "no_upstream"]);
    assert_eq!(body["video_dies_at"], json!("decode_to_fanout"));
}

#[test]
fn gs_a_deaf_link_is_located_at_rf_to_decode_with_the_cause() {
    // Nothing decodes (packets/s zero) and the link cause is surfaced — the fault is
    // the very first hop. mediamtx management API is auth-gated on a ground station
    // (bytes unreadable → None), the real condition the served hop degrades under.
    let dir = tempfile::tempdir().unwrap();

    let deaf = json!({ "packets_received": 0, "link_diag": "deaf", "fanout_forwarded": 5_000 });
    write_wfb_sidecar(dir.path(), deaf.clone());
    let s0 = VideoSample::from_run_dir(dir.path(), None, true);
    write_wfb_sidecar(dir.path(), deaf);
    let s1 = VideoSample::from_run_dir(dir.path(), None, true);

    let body = build_video_diagnostics("ground-station", &Some("direct".into()), &s0, &s1);

    assert_eq!(body["video_dies_at"], json!("rf_to_decode"));
    let hops = body["hops"].as_array().unwrap();
    assert_eq!(hops[0]["verdict"], json!("stalled"));
    // The legible cause rides the hop detail so the operator sees WHY, not just where.
    assert_eq!(hops[0]["detail"], json!("link_diag=deaf"));
}

#[test]
fn gs_a_missing_sidecar_degrades_to_unknown_at_the_first_read_never_panics() {
    // The canonical wfb-stats object is absent entirely (the service never wrote it):
    // the sidecar-driven hops read `unknown` (source unreadable), the verifier stops
    // the diagnosis at the first unreadable hop, and nothing panics on the real read.
    let dir = tempfile::tempdir().unwrap();
    // No wfb-stats.json in `dir`.
    let s0 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);
    let s1 = VideoSample::from_run_dir(dir.path(), Some(2_000), true);

    let body = build_video_diagnostics("ground-station", &Some("direct".into()), &s0, &s1);

    assert_eq!(body["hops"][0]["verdict"], json!("unknown"));
    assert_eq!(body["video_dies_at"], json!("rf_to_decode"));
}

// --- drone (video SOURCE): camera → mediamtx → radio TX ------------------------

#[test]
fn drone_every_hop_flows_while_camera_bytes_and_tx_advance() {
    // mediamtx `main` ingest ADVANCES (camera → encoder → mediamtx alive) and the
    // wfb TX injection rate is positive (the tap → wfb_tx leg alive).
    let dir = tempfile::tempdir().unwrap();
    write_wfb_sidecar(dir.path(), json!({ "tx_bytes_per_s": 4_000 }));
    let s0 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);
    let s1 = VideoSample::from_run_dir(dir.path(), Some(2_000), true);

    let body = build_video_diagnostics("drone", &None, &s0, &s1);

    assert_eq!(body["profile"], json!("drone"));
    assert_eq!(body["role"], Value::Null);
    assert_eq!(
        hop_names(&body),
        vec!["camera_to_mediamtx", "mediamtx_to_radio_tx"]
    );
    assert_eq!(verdicts(&body), vec!["flowing", "flowing"]);
    assert_eq!(body["video_dies_at"], Value::Null);
}

#[test]
fn drone_a_frozen_camera_is_located_at_camera_to_mediamtx() {
    // mediamtx `main` ingest is FLAT across the window (no camera / dead encoder).
    // With no frames reaching mediamtx the tap starves wfb_tx, so the TX injection
    // rate is zero too: the camera hop stalls and the radio hop, reading zero only
    // because its upstream already died, is `no_upstream` — not blamed.
    let dir = tempfile::tempdir().unwrap();
    write_wfb_sidecar(dir.path(), json!({ "tx_bytes_per_s": 0 }));
    let s0 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);
    let s1 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);

    let body = build_video_diagnostics("drone", &None, &s0, &s1);

    assert_eq!(verdicts(&body), vec!["stalled", "no_upstream"]);
    assert_eq!(body["video_dies_at"], json!("camera_to_mediamtx"));
}

#[test]
fn drone_a_dead_tx_injection_is_located_at_the_radio_hop() {
    // Camera fine (mediamtx advances) but the wfb TX injection rate is zero — the
    // tap → wfb_tx leg died. This is the exact bench-bug shape the verifier guards:
    // the radio hop reads `tx_bytes_per_s` (the injection rate), NOT `bitrate_kbps`
    // (the RX-decode rate, which is always 0 on a source drone), so a healthy camera
    // with a dead transmitter is attributed to the radio hop, not falsely to a stall
    // nobody can see.
    let dir = tempfile::tempdir().unwrap();
    write_wfb_sidecar(dir.path(), json!({ "tx_bytes_per_s": 0 }));
    let s0 = VideoSample::from_run_dir(dir.path(), Some(1_000), true);
    write_wfb_sidecar(dir.path(), json!({ "tx_bytes_per_s": 0 }));
    let s1 = VideoSample::from_run_dir(dir.path(), Some(2_000), true);

    let body = build_video_diagnostics("drone", &None, &s0, &s1);

    assert_eq!(verdicts(&body), vec!["flowing", "stalled"]);
    assert_eq!(body["video_dies_at"], json!("mediamtx_to_radio_tx"));
}
