//! Preview the live install progress UI without running an install.
//!
//!   cargo run -p ados-installer --example ui_demo
//!
//! Drives the real renderer through the public progress-sink API with a scripted
//! step sequence, so you can see the checklist, spinner, download bar, and the
//! closing success card in your own terminal. Add `--fail` to preview the
//! failure panel.

use std::thread::sleep;
use std::time::Duration;

use ados_installer::graph::StepOutcome;
use ados_installer::ui::{self, ProgressSink, RenderMode, SummaryData, Theme};

fn step(sink: &ProgressSink, id: &str, ms: u64) {
    sink.step_started(id);
    sleep(Duration::from_millis(ms));
    sink.step_result(id, &StepOutcome::Ok);
}

/// A step that streams a few headline + raw-log lines to preview the live pane.
fn step_streamed(sink: &ProgressSink, id: &str, lines: &[&str], ms: u64) {
    sink.step_started(id);
    for l in lines {
        sink.activity(id, l.to_string());
        sink.sub_log(id, l);
        sleep(Duration::from_millis(ms));
    }
    sink.step_result(id, &StepOutcome::Ok);
}

fn main() {
    let fail = std::env::args().any(|a| a == "--fail");
    let theme = Theme::detect(false, false);
    let header = "Installing the ADOS Drone Agent (drone)…".to_string();
    // Preview the full-screen renderer when a controlling terminal is reachable.
    let (mode, tty) = ui::resolve_live_mode(RenderMode::Rich, None);
    let (sink, render) = ui::start(mode, header, theme, tty);

    // Checking system: preflight runs, purge_residue is a cached no-op.
    step(&sink, "preflight", 250);
    sink.step_result("purge_residue", &StepOutcome::Skipped);

    step_streamed(
        &sink,
        "deps",
        &[
            "unpacking gstreamer1.0-tools",
            "unpacking ffmpeg",
            "configuring avahi-daemon",
        ],
        300,
    );
    step_streamed(
        &sink,
        "venv_agent",
        &["cloning repository", "building agent package"],
        400,
    );
    step_streamed(
        &sink,
        "wfb_ng",
        &["compiling radio stack", "installing radio stack"],
        350,
    );

    // Downloading components: a determinate k-of-N bar with per-file byte
    // progress + named components in the detail pane.
    sink.step_started("fetch_binaries");
    let services = [
        "ados-supervisor",
        "ados-mavlink-router",
        "ados-video",
        "ados-cloud",
        "ados-vision",
        "mediamtx",
    ];
    let total = services.len() as u64;
    let bytes = 8_388_608u64;
    for (i, svc) in services.iter().enumerate() {
        sink.activity("fetch_binaries", format!("installing {svc}"));
        for s in 0..=8u64 {
            sink.byte_progress("fetch_binaries", bytes * s / 8, bytes, svc);
            sleep(Duration::from_millis(70));
        }
        sink.sub_log("fetch_binaries", &format!("✓ {svc} 8.0 MB"));
        sink.sub_progress("fetch_binaries", (i as u64) + 1, total);
    }
    sink.step_result("fetch_binaries", &StepOutcome::Ok);

    sink.step_result("dkms", &StepOutcome::Skipped);
    step(&sink, "config_identity", 350);
    step(&sink, "network_mac_pin", 200);
    sink.step_result("rtl_regulatory", &StepOutcome::Skipped);
    step(&sink, "watchdog", 150);

    if fail {
        sink.step_started("systemd");
        sleep(Duration::from_millis(500));
        sink.step_result(
            "systemd",
            &StepOutcome::Failed("unit failed to start".into()),
        );
        // Downstream skipped by the engine; mirror that here.
        sink.step_result("start", &StepOutcome::Skipped);
        sink.step_result("health", &StepOutcome::Skipped);
        sink.summary(summary("failed", &["systemd"]));
    } else {
        step(&sink, "systemd", 500);
        step(&sink, "start", 1500);
        step(&sink, "health", 600);
        sink.summary(summary("ok", &[]));
    }
    sink.finish();
    render.finish();
}

fn summary(status: &str, required: &[&str]) -> SummaryData {
    SummaryData {
        status: status.to_string(),
        version: "0.51.5".to_string(),
        profile: "drone".to_string(),
        board: "Raspberry Pi 4 Model B".to_string(),
        device_id: "17bf646b".to_string(),
        hostname: "skynode".to_string(),
        setup_url: "http://skynode.local:8080/setup".to_string(),
        lan_ips: vec!["192.168.1.42".to_string(), "10.0.0.7".to_string()],
        paired: true,
        failed_steps: required.iter().map(|s| s.to_string()).collect(),
        required_failures: required.iter().map(|s| s.to_string()).collect(),
    }
}
