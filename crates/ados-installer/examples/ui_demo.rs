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

fn main() {
    let fail = std::env::args().any(|a| a == "--fail");
    let theme = Theme::detect(false, false);
    let header = "Installing the ADOS Drone Agent (drone)…".to_string();
    let (sink, render) = ui::start(RenderMode::Rich, header, theme);

    // Checking system: preflight runs, purge_residue is a cached no-op.
    step(&sink, "preflight", 250);
    sink.step_result("purge_residue", &StepOutcome::Skipped);

    step(&sink, "deps", 1300);
    step(&sink, "venv_agent", 900);
    step(&sink, "wfb_ng", 800);

    // Downloading components: a determinate k-of-N bar.
    sink.step_started("fetch_binaries");
    let total = 15u64;
    for i in 0..=total {
        sink.sub_progress("fetch_binaries", i, total);
        sleep(Duration::from_millis(110));
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
        paired: true,
        failed_steps: required.iter().map(|s| s.to_string()).collect(),
        required_failures: required.iter().map(|s| s.to_string()).collect(),
    }
}
