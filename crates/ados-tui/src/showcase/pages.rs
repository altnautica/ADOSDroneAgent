//! Resources for the Logs, Extensions, System, Settings and Ground frames.

use serde_json::{json, Value};

use super::fixtures::{wave, LAN_IP};
use crate::poll::ResourceId;
use crate::stream::LogEntry;

/// `level|logger|message`, oldest first.
const LOG: &str = "\
INFO|ados.supervisor|starting the agent, profile drone
INFO|ados.discovery|mDNS advertising my-drone.local
INFO|ados.api|listening on :8080
INFO|ados.mavlink|serial /dev/ttyACM0 opened at 115200 baud
INFO|ados.mavlink|router up: tcp :5760, websocket :8765
INFO|ados.health|hardware check: 4 ok
INFO|ados.vision|model loaded, 30 fps budget
INFO|ados.logd|journal capture on
INFO|ados.supervisor|all 11 services running
INFO|ados.mavlink|heartbeat from sysid 1 (ArduPilot copter)
INFO|ados.mavlink|parameter sweep complete: 1043 parameters
INFO|ados.video|encoder started: h264 1280x720 30 fps 6000 kbit/s
INFO|ados.wfb|wlan1 up on channel 149, MCS 1
INFO|ados.api|paired session opened from Mission Control
WARNING|ados.health|cpu_thermal 61.2 °C above the 60 °C notice level
INFO|ados.health|cpu_thermal back to 55.8 °C
INFO|ados.mavlink|mode STABILIZE
INFO|ados.cloud|telemetry relay connected
WARNING|ados.wfb|loss 2.1 % over the last 5 s
INFO|ados.wfb|loss back under 1 %
INFO|ados.video|viewer joined over WHEP
ERROR|ados.plugins|com.example.gimbal: install refused, signature unknown
INFO|ados.plugins|com.example.follow 0.2.10 running
INFO|ados.mavlink|GPS 3D fix, 14 satellites
INFO|ados.api|config write discovery.mdns_enabled = true
INFO|ados.mavlink|EKF origin set
INFO|ados.mavlink|home position set
INFO|ados.cloud|heartbeat acknowledged
INFO|ados.video|bitrate 6000 kbit/s held, link healthy
INFO|ados.wfb|RSSI -52 dBm, SNR 28 dB
INFO|ados.health|all checks passing";

/// Log lines as the stream delivers them.
pub(super) fn log_lines() -> Vec<LogEntry> {
    LOG.lines()
        .enumerate()
        .filter_map(|(i, row)| {
            let mut parts = row.splitn(3, '|');
            let (level, logger, msg) = (parts.next()?, parts.next()?, parts.next()?);
            let s = 10 + i * 2;
            let ts = format!(
                "2026-10-21T12:{:02}:{:02}.{:03}Z",
                4 + s / 60,
                s % 60,
                37 * i % 1000
            );
            LogEntry::from_value(&json!({
                "seq": i + 1, "timestamp": ts, "level": level, "logger": logger, "message": msg,
            }))
        })
        .collect()
}

const FOLLOW: &str = "com.example.follow";

pub(super) fn extensions() -> Vec<(ResourceId, Value)> {
    vec![
        (
            ResourceId::Plugins,
            json!({"installs": [{
                "plugin_id": FOLLOW, "version": "0.2.10", "status": "running", "signer_id": "example",
                "failure_reason": null, "revocation_pending": false,
                "permissions": {
                    "mavlink.read": {"granted": true, "granted_at": 1},
                    "mavlink.command": {"granted": true, "granted_at": 1},
                    "video.frames": {"granted": true, "granted_at": 1}
                }
            }]}),
        ),
        (
            ResourceId::PluginsCatalog,
            json!({"plugins": [
                {"id": FOLLOW, "name": "Follow", "version": "0.2.10", "risk": "high",
                 "download_url": "https://example.com/f.adosplug", "archive_sha256": "aa"},
                {"id": "com.example.gimbal", "name": "Gimbal Controller", "version": "1.4.1", "risk": "medium",
                 "download_url": "https://example.com/g.adosplug", "archive_sha256": "bb"},
                {"id": "com.example.survey", "name": "Survey Grid", "version": "0.9.0", "risk": "low",
                 "download_url": "https://example.com/s.adosplug", "archive_sha256": "cc"}
            ]}),
        ),
        (
            ResourceId::PluginDetail(FOLLOW.into()),
            json!({
                "install": {},
                "manifest": {
                    "id": FOLLOW, "version": "0.2.10", "name": "Follow Subject", "risk": "high",
                    "permissions": [
                        {"id": "mavlink.read", "required": true, "label": "Read MAVLink"},
                        {"id": "mavlink.command", "required": true, "label": "Send guided targets"},
                        {"id": "video.frames", "required": true, "label": "Read camera frames"}
                    ],
                    "mcp": {"tools": [
                        {"name": "follow.start", "safety_class": "flight"},
                        {"name": "follow.stop", "safety_class": "flight"}
                    ], "resources": [], "prompts": []},
                    "gcs": {"contributes": {
                        "parameters": [
                            {"key": "distance_m", "schema": {"type": "number", "minimum": 3, "maximum": 30, "default": 8},
                             "ui": {"label": "Follow distance", "group": "Follow"}},
                            {"key": "height_m", "schema": {"type": "number", "minimum": 2, "maximum": 40, "default": 10},
                             "ui": {"label": "Follow height", "group": "Follow"}}
                        ],
                        "settings": []
                    }}
                }
            }),
        ),
        (
            ResourceId::PluginConfig(FOLLOW.into()),
            json!({"plugin_id": FOLLOW, "values": {"distance_m": 12, "height_m": 10}}),
        ),
        (
            ResourceId::PluginReadiness(FOLLOW.into()),
            json!({"ok": true, "plugin_id": FOLLOW, "services": [
                {"name": "tracker", "ready": true, "reason": null},
                {"name": "planner", "ready": true, "reason": null}
            ]}),
        ),
        (
            ResourceId::PluginState(FOLLOW.into()),
            json!({"follow.state": {"payload": {"active": false, "subject": "none"}, "ts_ms": 5}}),
        ),
    ]
}

pub(super) fn system_load(i: usize) -> Value {
    json!({
        "cpu_percent": wave(i, 24.0, 12.0), "cpu_count": 4,
        "memory_total_mb": 4096, "memory_used_mb": 1730, "memory_percent": 42.2,
        "disk_total_gb": 32.0, "disk_used_gb": 9.4, "disk_percent": 29.4,
        "swap_total_mb": 0, "swap_percent": 0.0,
        "temperatures": {"cpu_thermal": wave(i, 54.0, 2.0), "gpu_thermal": 52.5}
    })
}

pub(super) fn system() -> Vec<(ResourceId, Value)> {
    let svc = |name: &str, mb: f64| json!({"name": name, "active": true, "state": "active", "sub_state": "running", "memory_mb": mb});
    vec![
        (
            ResourceId::Services,
            json!({"systemd_available": true, "services": [
                svc("ados-supervisor", 18.2), svc("ados-api", 46.9), svc("ados-mavlink", 21.4),
                svc("ados-video", 38.5), svc("ados-wfb", 9.8), svc("ados-cloud", 24.1),
                svc("ados-health", 12.6), svc("ados-plugins", 15.3), svc("ados-discovery", 6.2),
                svc("ados-vision", 61.7), svc("ados-logd", 8.4)
            ]}),
        ),
        (
            ResourceId::Peripherals,
            json!({"peripherals": [
                {"id": "ados.fc", "display_name": "Flight controller", "connected": true, "transport": "usb"},
                {"id": "ados.gps", "display_name": "GNSS receiver", "connected": true, "transport": "usb"},
                {"id": "ados.radio", "display_name": "RTL8812AU radio", "connected": true, "transport": "usb"},
                {"id": "ados.camera", "display_name": "Main camera", "connected": true, "transport": "csi"}
            ], "count": 4}),
        ),
        (
            ResourceId::HardwareCheck,
            json!({"items": [
                {"id": "fc", "label": "Flight controller", "required": true, "state": "ok", "detail": "ArduPilot copter on /dev/ttyACM0"},
                {"id": "cam", "label": "Camera", "required": true, "state": "ok", "detail": "CSI camera 1920×1080"},
                {"id": "radio", "label": "Video radio", "required": true, "state": "ok", "detail": "RTL8812AU on wlan1"},
                {"id": "gps", "label": "GNSS", "required": false, "state": "ok", "detail": "3D fix, 14 satellites"}
            ]}),
        ),
    ]
}

pub(super) fn wifi() -> Vec<(ResourceId, Value)> {
    vec![
        (
            ResourceId::WifiStatus,
            json!({"connected": true, "ssid": "Field-Net", "signal": 78, "ip": LAN_IP, "gateway": null, "security": "WPA2"}),
        ),
        (
            ResourceId::WifiConfigured,
            json!({"connections": [
                {"name": "Field-Net", "type": "802-11-wireless", "device": "wlan0", "autoconnect": true},
                {"name": "Workshop", "type": "802-11-wireless", "device": null, "autoconnect": false}
            ]}),
        ),
    ]
}

pub(super) fn wifi_scan() -> Value {
    json!({"networks": [
        {"ssid": "Field-Net", "bssid": "aa", "signal": 78, "security": "WPA2", "in_use": true},
        {"ssid": "Hangar-5G", "bssid": "bb", "signal": 66, "security": "WPA2", "in_use": false},
        {"ssid": "Workshop", "bssid": "cc", "signal": 51, "security": "WPA3", "in_use": false},
        {"ssid": "Visitor", "bssid": "dd", "signal": 34, "security": "--", "in_use": false},
        {"ssid": "Ops-Van", "bssid": "ee", "signal": 17, "security": "WPA2", "in_use": false}
    ]})
}

pub(super) fn mcp() -> Vec<(ResourceId, Value)> {
    vec![
        (
            ResourceId::McpStatus,
            json!({"accept_enabled": true, "any_minted": true, "tokens": [
                {"token_id": "t1", "label": "laptop", "scopes": ["read", "flight"], "created_at": 1_792_454_400.0,
                 "expires_at": 1_795_046_400_000_i64, "revoked": false, "expired": false}
            ]}),
        ),
        (
            ResourceId::SetupStatus,
            json!({"network": {"mdns_host": "my-drone.local", "api_port": 8080}}),
        ),
    ]
}

pub(super) fn ground() -> Vec<(ResourceId, Value)> {
    vec![
        (
            ResourceId::Gnss,
            json!({
                "present": true,
                "receivers": [{"id": "gnss0", "source": "hub", "model": "M10", "fix": "rtk_fixed", "sats": 14,
                               "hdop": 0.8, "lat_deg": 47.397742, "lon_deg": 8.545594, "rtk": "fixed", "pps_locked": true}],
                "base": {"mode": "survey", "state": "surveying", "elapsed_s": 180, "min_dur_s": 300,
                         "accuracy_m": 2.4, "target_accuracy_m": 2.0, "fixed": null,
                         "rtcm_out": {"msgs_per_s": 1.0, "bytes_total": 18432, "targets": 2}},
                "heading": {"available": false, "deg": null, "accuracy_deg": null, "baseline_m": null}
            }),
        ),
        (
            ResourceId::Gamepads,
            json!({
                "devices": [
                    {"device_id": "usb:045e:028e:event3", "name": "Pad One", "connected": true},
                    {"device_id": "usb:1209:4f54:event5", "name": "RC Sticks", "connected": true}
                ],
                "primary_id": "usb:045e:028e:event3"
            }),
        ),
        (
            ResourceId::BtPaired,
            json!({"devices": [
                {"device_id": "bt:aa", "mac": "AA:BB:CC:DD:EE:FF", "name": "Headset", "connected": true},
                {"device_id": "bt:bb", "mac": "AA:BB:CC:DD:EE:01", "name": "Field tablet", "connected": false}
            ]}),
        ),
        (
            ResourceId::Invites,
            json!({"pending": [{"invite_id": "abc", "label": "Phone", "phone_fingerprint": "ABCD-EF01-2345-6789",
                                "expires_at_ms": super::fixtures::now_ms() + 540_000}]}),
        ),
        (
            ResourceId::GsDisplay,
            json!({"resolution": "auto", "kiosk_enabled": true, "kiosk_target_url": null}),
        ),
    ]
}
