use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::app::{App, Slot, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, feed, test_app};

fn put(app: &mut App, id: ResourceId, value: Value) {
    app.core.res.insert(
        id,
        Slot {
            value: Some(value),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
}

fn gs_app() -> App {
    let mut app = test_app();
    let status = json!({
        "version": "0.99.108", "device_name": "ados-gs", "profile": "ground_station",
        "paired": true, "mavlink": {"connected": false}
    });
    feed(&mut app, status, ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Vehicle);
    put(
        &mut app,
        ResourceId::Gnss,
        json!({
            "present": true,
            "receivers": [{"id": "gnss0", "source": "hub", "model": "M10", "fix": "rtk_fixed", "sats": 14,
                           "hdop": 0.8, "lat_deg": 12.97, "lon_deg": 77.59, "rtk": "fixed", "pps_locked": true}],
            "base": {"mode": "survey", "state": "surveying", "elapsed_s": 120, "min_dur_s": 300,
                     "accuracy_m": 3.1, "target_accuracy_m": 2.0, "fixed": null,
                     "rtcm_out": {"msgs_per_s": 1.0, "bytes_total": 10, "targets": 2}},
            "heading": {"available": false, "deg": null, "accuracy_deg": null, "baseline_m": null}
        }),
    );
    put(
        &mut app,
        ResourceId::Gamepads,
        json!({
            "devices": [{"device_id": "usb:045e:028e:event3", "name": "Pad One", "connected": true}],
            "primary_id": "usb:045e:028e:event3"
        }),
    );
    put(
        &mut app,
        ResourceId::BtPaired,
        json!({
            "devices": [{"device_id": "bt:aa", "mac": "AA:BB:CC:DD:EE:FF", "name": "Headset", "connected": true}]
        }),
    );
    put(
        &mut app,
        ResourceId::Invites,
        json!({
            "pending": [{"invite_id": "abc", "label": "Phone", "phone_fingerprint": "ABCD-EF01-2345-6789",
                         "expires_at_ms": 0}]
        }),
    );
    put(
        &mut app,
        ResourceId::GsDisplay,
        json!({"resolution": "auto", "kiosk_enabled": true, "kiosk_target_url": null}),
    );
    app
}

#[test]
fn ground_renders_gnss_inputs_invites_and_display() {
    let mut app = gs_app();
    let text = draw(&mut app, 120, 40);
    for want in [
        "GNSS",
        "gnss0",
        "rtk fixed",
        "survey · surveying",
        "120/300 s",
        "RTCM 1.0 msg/s → 2 targets",
        "Pad One",
        "PRIMARY",
        "Headset",
        "connected",
        "ABCD-EF01-2345-6789",
        "Resolution auto",
        "Kiosk on",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn an_absent_gnss_sidecar_says_so() {
    let mut app = gs_app();
    put(
        &mut app,
        ResourceId::Gnss,
        json!({"present": false, "receivers": [], "base": null, "heading": null}),
    );
    let text = draw(&mut app, 120, 40);
    assert!(text.contains("No GNSS receiver reporting"), "{text}");
}
