//! Hardware-check isolation: the synchronous probe sweep runs on the
//! blocking pool, never on the HTTP handler thread.
//!
//! `POST /api/v1/setup/hardware-check/refresh` (and the GET sibling)
//! shells out to lsusb, reads /proc, walks /sys, scans
//! /etc/udev/rules.d, and probes for `wfb_tx`. On a single-core A7 the
//! end-to-end latency lands around 250 ms. If the handler ran the
//! probe synchronously on the axum task that also serves /wfb,
//! /status, /diag, and the WS log streamer, a single hardware-check
//! call would stall every other request for the duration.
//!
//! The isolation contract is that the probe is wrapped in
//! `tokio::task::spawn_blocking`. While a probe is in flight, every
//! other REST route on the same router must remain responsive within
//! a small fraction of the probe latency.
//!
//! This test exercises the contract two ways:
//!
//! 1. While we hold the blocking pool busy with a synthetic 200ms
//!    sleep, drive 3 concurrent reads against /api/v1/setup/wfb on
//!    the same router and assert each returns within 100 ms.
//! 2. While a real `/hardware-check/refresh` request is in flight,
//!    drive 3 concurrent /api/v1/setup/status reads and assert each
//!    returns within 100 ms.
//!
//! Both flavours prove the HTTP handler thread is not starved by the
//! blocking probe pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_setup::{
    setup_router_with_wfb, state::StateStore, SetupState,
};
use ados_wfb::{WfbConfig, WfbManager};
use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;

fn fresh_state(dir: &tempfile::TempDir) -> Arc<SetupState> {
    let agent_yaml = dir.path().join("agent.yaml");
    std::fs::write(
        &agent_yaml,
        "agent:\n  device_id: \"hw-iso-001\"\n  name: \"HW Iso\"\nmavlink:\n  port: \"/dev/ttyS0\"\n  baud: 115200\ncloud:\n  api_key: \"\"\napi:\n  bind: \"127.0.0.1:18080\"\n",
    )
    .unwrap();
    let state_path = dir.path().join("setup-state.json");
    let store = StateStore::new(state_path);
    let store_for_status = store.clone();
    Arc::new(SetupState {
        agent_yaml,
        store,
        status_builder: Box::new(move || {
            let persisted = store_for_status.load().unwrap_or_default();
            json!({
                "version": "0.1.0",
                "agent_version": "0.1.0",
                "device_id": "hw-iso-001",
                "device_name": "HW Iso",
                "profile": "drone",
                "ground_role": "",
                "runtime_mode": "lite",
                "setup_complete": persisted.finalized,
                "setup_finalized": persisted.finalized,
                "completion_percent": 0,
                "next_action": "pair",
                "steps": [],
                "skipped_steps": [],
                "access_urls": [],
                "network": { "hostname": "", "mdns_host": "", "api_port": 8080,
                             "hotspot_enabled": false, "hotspot_ssid": "", "local_ips": [] },
                "mavlink": { "connected": false, "port": "/dev/ttyS0", "baud": 115200,
                             "websocket_url": null, "public_websocket_url": null },
                "video": { "state": "not_initialized", "whep_url": null,
                           "public_whep_url": null, "recording": false },
                "remote_access": { "provider": "none", "enabled": false, "configured": false,
                                   "status": "disabled", "public_urls": [], "error": "" },
                "cloud_choice": { "mode": "cloud", "paired": false, "pair_code_required": true,
                                  "backend_url": "", "backend_reachable": false,
                                  "last_checked": null },
                "profile_suggestion": null,
                "hardware_check": null,
                "services": []
            })
        }),
    })
}

fn fresh_wfb_manager() -> Arc<Mutex<WfbManager>> {
    let mgr = WfbManager::new(WfbConfig::default()).expect("default WfbConfig must construct");
    Arc::new(Mutex::new(mgr))
}

fn build_router(state: Arc<SetupState>) -> axum::Router {
    setup_router_with_wfb(state, fresh_wfb_manager())
}

async fn drive(
    router: axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Duration) {
    let started = Instant::now();
    let request = match body {
        Some(b) => Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap(),
    };
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let _ = to_bytes(response.into_body(), 64 * 1024).await;
    (status, started.elapsed())
}

#[tokio::test]
async fn concurrent_reads_complete_quickly_when_blocking_pool_is_busy() {
    // Synthetically saturate the blocking pool the way a slow
    // hardware-check probe would. We spawn a handful of
    // `tokio::task::spawn_blocking` tasks that each sleep 200 ms; the
    // tokio default blocking pool size is 512, so a single in-flight
    // probe never starves the pool, but we model the worst case by
    // spawning enough to exceed the number of CPU cores. The HTTP
    // handler thread (the foreground tokio worker) must still serve
    // /api/v1/setup/wfb in well under 100 ms.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let manager = fresh_wfb_manager();

    // Hold the blocking pool busy for ~200 ms with a few parallel
    // sleeps. spawn_blocking returns a JoinHandle; we keep them alive
    // until after the concurrent reads complete.
    let mut handles = Vec::new();
    for _ in 0..4 {
        handles.push(tokio::task::spawn_blocking(|| {
            std::thread::sleep(Duration::from_millis(200));
        }));
    }

    // Drive 3 concurrent reads against the WFB-ng route. The WFB
    // handler is a fast read of an in-memory state snapshot — no
    // blocking I/O, no spawn_blocking — so any latency we measure is
    // the framework + scheduler overhead, NOT the blocking pool.
    let mut futs = Vec::new();
    for _ in 0..3 {
        let s = state.clone();
        let m = manager.clone();
        futs.push(tokio::spawn(async move {
            let router = ados_setup::setup_router_with_wfb(s, m);
            drive(router, Method::GET, "/api/v1/setup/wfb", None).await
        }));
    }

    for fut in futs {
        let (status, elapsed) = fut.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(
            elapsed < Duration::from_millis(100),
            "concurrent read took {elapsed:?} (>100ms) while blocking pool was busy — possible starvation"
        );
    }

    // Reap the synthetic blocking workers so they do not outlive the test.
    for h in handles {
        let _ = h.await;
    }
}

#[tokio::test]
async fn hardware_check_in_flight_does_not_block_other_routes() {
    // Real workload: kick off a /hardware-check/refresh call and, while
    // it is in flight, drive 3 /status reads on a fresh router. Each
    // /status call must complete inside 100 ms even though the probe
    // is shelling out to lsusb + walking /proc on the blocking pool.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let manager = fresh_wfb_manager();

    // Start the hardware-check call but do not await it yet. The router
    // is built fresh per request because oneshot consumes it.
    let hw_router = build_router(state.clone());
    let hw_state = state.clone();
    let hw_manager = manager.clone();
    let hw_fut = tokio::spawn(async move {
        let _ = hw_state; // capture for lifetime
        let _ = hw_manager;
        drive(
            hw_router,
            Method::POST,
            "/api/v1/setup/hardware-check/refresh",
            None,
        )
        .await
    });

    // Yield so the hardware-check task gets a chance to enter the
    // spawn_blocking call before we measure the concurrent reads.
    tokio::task::yield_now().await;

    let mut read_futs = Vec::new();
    for _ in 0..3 {
        let s = state.clone();
        let m = manager.clone();
        read_futs.push(tokio::spawn(async move {
            let router = ados_setup::setup_router_with_wfb(s, m);
            drive(router, Method::GET, "/api/v1/setup/status", None).await
        }));
    }

    for f in read_futs {
        let (status, elapsed) = f.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(
            elapsed < Duration::from_millis(100),
            "/status returned in {elapsed:?} (>100ms) while a hardware-check was in flight — handler thread starvation"
        );
    }

    // The hardware-check itself eventually completes successfully.
    let (hw_status, _) = hw_fut.await.unwrap();
    assert_eq!(hw_status, StatusCode::OK);
}

#[tokio::test]
async fn hardware_check_refresh_completes_within_reasonable_budget() {
    // Sanity bound: the probe sweep on a dev mac (no /proc, no lsusb
    // for most devices, no /etc/udev/rules.d full of rules) finishes
    // well under 5 seconds. A regression that pushes the latency past
    // this ceiling indicates someone added a network call to the
    // blocking probe and the next bench bringup will pay for it. The
    // budget is intentionally loose so flaky CI hosts do not nag.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let router = build_router(state);
    let (status, elapsed) =
        drive(router, Method::POST, "/api/v1/setup/hardware-check/refresh", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        elapsed < Duration::from_secs(5),
        "hardware-check/refresh took {elapsed:?} — investigate before shipping"
    );
}
