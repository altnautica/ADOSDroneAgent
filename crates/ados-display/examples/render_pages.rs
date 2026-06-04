//! Render every LCD page to a PNG for visual parity review.
//!
//! Builds two fully-populated [`PageContext`] fixtures — a paired, link-up
//! ground station with live drone / mesh / uplink data, and a fresh-boot
//! unpaired variant — then paints every registered page (through its chrome)
//! and writes the 480x320 frame to `target/ui-parity/<page>_<variant>.png`.
//!
//! Run on the host toolchain (the framebuffer write path is not exercised here,
//! only the pure page composers):
//!
//! ```text
//! cargo run -p ados-display --example render_pages
//! ```

use std::path::{Path, PathBuf};

use ados_display::graphics::palette::{Palette, DARK};
use ados_display::graphics::primitives::Canvas;
use ados_display::pages::{
    about::AboutDetailPage, channel_hops::ChannelHopsPage, dashboard::DashboardPage,
    diagnostics::DiagnosticsDetailPage, drone::DroneDetailPage, link_stats::LinkStatsPage,
    mesh::MeshDetailPage, more::MorePage, pair_drone::PairDroneDetailPage,
    radio_link::RadioLinkDetailPage, settings::SettingsPage, uplink::UplinkDetailPage,
    video::VideoPage, CloudCtx, DeviceCtx, DiagnosticsCtx, DroneCtx, FcCtx, HardwareItem,
    HealthCtx, HopEntry, HoppingCtx, LinkCtx, MeshCtx, MeshPeer, NetworkCtx, Page, PageContext,
    PairedDroneCtx, PairingCtx, RadioCtx, RoleCtx, SettingsCtx, SettingsRow, SystemCtx, UplinkCtx,
    VideoCtx, WifiClientCtx,
};

/// Build a 60-sample series that wanders around `base` with amplitude `amp`, so
/// the sparkline / battery-trend surfaces have a believable wave to draw rather
/// than a flat line.
fn wave(base: f64, amp: f64) -> Vec<Option<f64>> {
    (0..60)
        .map(|i| {
            let t = i as f64 / 6.0;
            Some(base + amp * (t.sin() * 0.6 + (t * 0.37).cos() * 0.4))
        })
        .collect()
}

/// A paired, link-up ground station with a live drone, an up mesh, a reachable
/// cellular uplink, and a cloud relay bound to Mission Control. This is the
/// "everything connected" frame every status page should render richly.
fn connected_context() -> PageContext {
    PageContext {
        hostname: "groundnode".to_string(),
        clock: "14:32:07".to_string(),
        setup_finalized: true,
        completion_percent: Some(100.0),
        next_action: None,
        link: LinkCtx {
            state: Some("connected".to_string()),
            rssi_dbm: Some(-58.0),
            snr_db: Some(27.5),
            noise_dbm: Some(-92.0),
            loss_percent: Some(0.8),
            bitrate_mbps: Some(18.4),
            bitrate_kbps: Some(18_400.0),
            fec_recovered: Some(214),
            fec_lost: Some(3),
            channel: Some(149),
            frequency_mhz: Some(5745),
            bandwidth_mhz: Some(20),
            tx_power_dbm: Some(22),
            mcs_index: Some(5),
            fec_k: Some(8),
            fec_n: Some(10),
            adaptive_bitrate_enabled: Some(true),
            recommended_tier_name: Some("high".to_string()),
            packets_received: Some(1_482_910),
            packets_lost: Some(1_204),
            rssi_history: wave(-58.0, 6.0),
        },
        radio: RadioCtx {
            topology: Some("external_5v".to_string()),
        },
        drone: DroneCtx {
            device_id: Some("ados-58c27faf".to_string()),
            fc_mode: Some("LOITER".to_string()),
            battery_pct: Some(78.0),
            gps_sats: Some(17),
            armed: Some(false),
            key_fingerprint: Some("3F:A2:91:0C:7E:4D".to_string()),
        },
        paired_drone: PairedDroneCtx {
            device_id: Some("ados-58c27faf".to_string()),
            key_fingerprint: Some("3F:A2:91:0C:7E:4D".to_string()),
            paired_at_seconds: Some(742.0),
            paired_at: Some(1_717_000_000.0),
        },
        fc: FcCtx {
            vehicle: Some("Multirotor".to_string()),
            mode: Some("LOITER".to_string()),
            armed: false,
            battery_voltage: Some(15.7),
            battery_remaining: Some(78.0),
            gps_fix_type: Some(4),
            gps_satellites_visible: Some(17),
            battery_history: wave(78.0, 5.0),
        },
        cloud: CloudCtx {
            paired: true,
            pair_code: Some("7QX4M2".to_string()),
            pairing_code: Some("7QX4M2".to_string()),
            latency_ms: Some(48.0),
            rtt_ms: Some(96.0),
            broadcasting: true,
            pair_url: Some("https://app.example.com/pair/7QX4M2".to_string()),
            mqtt_state: Some("connected".to_string()),
            http_state: Some("ok".to_string()),
            drone_id: Some("ados-58c27faf".to_string()),
        },
        pairing: PairingCtx {
            code: Some("7QX4M2".to_string()),
            pair_url: Some("http://groundnode.local:8080".to_string()),
            window_active: false,
            window_remaining_seconds: None,
        },
        role: RoleCtx {
            current: Some("relay".to_string()),
            configured: Some("relay".to_string()),
            mesh_capable: true,
        },
        mesh: MeshCtx {
            up: true,
            partition: false,
            peer_count: 2,
            selected_gateway: Some("ados-aa11bb22".to_string()),
            mesh_id: Some("alt-mesh-01".to_string()),
            peers: vec![
                MeshPeer {
                    device_id: Some("ados-aa11bb22".to_string()),
                    role: Some("direct".to_string()),
                    last_seen_seconds_ago: Some(2.0),
                },
                MeshPeer {
                    device_id: Some("ados-cc33dd44".to_string()),
                    role: Some("receiver".to_string()),
                    last_seen_seconds_ago: Some(9.0),
                },
            ],
        },
        network: NetworkCtx {
            ap_ssid: Some("ADOS-GS-9F2C".to_string()),
            ap_ip: Some("10.42.0.1".to_string()),
            usb_ip: Some("10.55.0.1".to_string()),
            uplink_type: Some("cellular".to_string()),
            uplink_reachable: true,
            mdns_host: Some("groundnode".to_string()),
            hotspot_ssid: Some("ADOS-GS-9F2C".to_string()),
            hotspot_enabled: true,
            wifi_client: WifiClientCtx {
                connected: true,
                ssid: Some("Ajay & Nidhi".to_string()),
                signal_dbm: Some(-52.0),
            },
        },
        uplink: UplinkCtx {
            modem_present: true,
            rsrp_dbm: Some(-84.0),
            rsrq_db: Some(-11.0),
            sinr_db: Some(14.0),
            band: Some("B40".to_string()),
            ip: Some("100.71.20.18".to_string()),
            tech: Some("LTE".to_string()),
            reason: None,
        },
        system: SystemCtx {
            cpu_pct: Some(31.0),
            ram_used_mb: Some(742.0),
            ram_total_mb: Some(3840.0),
            temp_c: Some(52.0),
            uptime_seconds: Some(7384.0),
            agent_version: Some("0.49.41".to_string()),
            cpu_history: wave(31.0, 18.0),
            temp_history: wave(52.0, 6.0),
        },
        hardware_check: vec![
            HardwareItem {
                id: Some("radio".to_string()),
                label: Some("WFB radio".to_string()),
                state: Some("ok".to_string()),
                fix_hint: None,
            },
            HardwareItem {
                id: Some("display".to_string()),
                label: Some("LCD panel".to_string()),
                state: Some("ok".to_string()),
                fix_hint: None,
            },
            HardwareItem {
                id: Some("uplink".to_string()),
                label: Some("Cellular modem".to_string()),
                state: Some("warning".to_string()),
                fix_hint: Some("Weak signal — reposition antenna".to_string()),
            },
        ],
        hopping: HoppingCtx {
            band: Some("u-nii-3".to_string()),
            radio_channel: Some(149),
            // Oldest-first, the order the hop supervisor writes its history in.
            history: vec![
                HopEntry {
                    at: 1_717_000_010.0,
                    from_channel: 157,
                    to_channel: 153,
                    ok: false,
                    trigger: Some("reactive".to_string()),
                },
                HopEntry {
                    at: 1_717_000_060.0,
                    from_channel: 153,
                    to_channel: 157,
                    ok: true,
                    trigger: Some("periodic".to_string()),
                },
                HopEntry {
                    at: 1_717_000_100.0,
                    from_channel: 157,
                    to_channel: 149,
                    ok: true,
                    trigger: Some("reactive".to_string()),
                },
            ],
        },
        video: VideoCtx {
            decoder: Some("h264 v4l2m2m".to_string()),
            active: true,
            recording: true,
            fps: Some(48.0),
            latency_ms: Some(62.0),
            bitrate_kbps: Some(4_180.0),
            mediamtx_ready: true,
            mediamtx_inbound_kbps: Some(4_096.0),
            camera_label: Some("USB UVC".to_string()),
            camera_count: 1,
        },
        health: HealthCtx {
            cpu_percent: Some(31.0),
            memory_percent: Some(19.3),
            disk_percent: Some(44.0),
            temperature: Some(52.0),
        },
        device: DeviceCtx {
            device_id: Some("ados-9f2c1a40".to_string()),
            device_name: Some("Ground Node".to_string()),
            version: Some("0.49.41".to_string()),
            board_name: Some("Reference SBC".to_string()),
            mac_eth0: Some("DC:A6:32:1A:2B:3C".to_string()),
            mac_wlan0: Some("DC:A6:32:1A:2B:3D".to_string()),
            primary_ip: Some("192.168.200.178".to_string()),
            primary_mac: Some("DC:A6:32:1A:2B:3C".to_string()),
            build_stamp: Some("2026-05-31T22:14:00Z".to_string()),
        },
        settings: SettingsCtx {
            rows: vec![
                SettingsRow {
                    id: "theme".to_string(),
                    label: "Theme".to_string(),
                    variant: "default".to_string(),
                    value: Some("Dark".to_string()),
                    toggle_on: None,
                },
                SettingsRow {
                    id: "hotspot".to_string(),
                    label: "WiFi hotspot".to_string(),
                    variant: "toggle".to_string(),
                    value: None,
                    toggle_on: Some(true),
                },
                SettingsRow {
                    id: "logging".to_string(),
                    label: "Log level".to_string(),
                    variant: "default".to_string(),
                    value: Some("INFO".to_string()),
                    toggle_on: None,
                },
                SettingsRow {
                    id: "rotation".to_string(),
                    label: "Display rotation".to_string(),
                    variant: "default".to_string(),
                    value: Some("0°".to_string()),
                    toggle_on: None,
                },
                SettingsRow {
                    id: "restart".to_string(),
                    label: "Restart agent".to_string(),
                    variant: "action".to_string(),
                    value: None,
                    toggle_on: None,
                },
            ],
            pending_reboot_count: 1,
            theme: Some("dark".to_string()),
            logging_level: Some("INFO".to_string()),
            display_rotation_degrees: Some(0),
            server_mode: Some("local".to_string()),
        },
        diagnostics: DiagnosticsCtx {
            agent_logs: vec![
                "INFO  supervisor: all services healthy".to_string(),
                "INFO  wfb: rssi=-58 dBm snr=27.5 dB ch=149".to_string(),
                "WARN  net: cellular signal weak (rsrp=-84)".to_string(),
                "INFO  mesh: 2 peers, gateway ados-aa11bb22".to_string(),
                "INFO  video: mediamtx path ready, 4.1 Mbps in".to_string(),
                "INFO  cloud: mqtt connected, rtt 96 ms".to_string(),
            ],
            log_scroll_offset: 0,
        },
    }
}

/// A fresh-boot, unpaired ground station: no link, no drone, the setup wizard
/// still open, an active local pairing window, and the cloud relay broadcasting
/// a pair code. Exercises every page's empty / waiting / unpaired branch.
fn unpaired_context() -> PageContext {
    PageContext {
        hostname: "ados-9f2c1a".to_string(),
        clock: "09:04:51".to_string(),
        setup_finalized: false,
        completion_percent: Some(38.0),
        next_action: Some("Plug in the WFB radio dongle".to_string()),
        link: LinkCtx {
            state: Some("unpaired".to_string()),
            rssi_history: Vec::new(),
            ..LinkCtx::default()
        },
        radio: RadioCtx {
            topology: Some("host_vbus".to_string()),
        },
        drone: DroneCtx::default(),
        paired_drone: PairedDroneCtx::default(),
        fc: FcCtx {
            battery_history: Vec::new(),
            ..FcCtx::default()
        },
        cloud: CloudCtx {
            paired: false,
            pair_code: Some("4KD9TZ".to_string()),
            pairing_code: Some("4KD9TZ".to_string()),
            broadcasting: true,
            pair_url: Some("https://app.example.com/pair/4KD9TZ".to_string()),
            mqtt_state: Some("connecting".to_string()),
            http_state: Some("connecting".to_string()),
            ..CloudCtx::default()
        },
        pairing: PairingCtx {
            code: Some("4KD9TZ".to_string()),
            pair_url: Some("http://ados-9f2c1a.local:8080".to_string()),
            window_active: true,
            window_remaining_seconds: Some(118.0),
        },
        role: RoleCtx {
            current: Some("direct".to_string()),
            configured: Some("direct".to_string()),
            mesh_capable: false,
        },
        mesh: MeshCtx {
            up: false,
            partition: false,
            peer_count: 0,
            selected_gateway: None,
            mesh_id: None,
            peers: Vec::new(),
        },
        network: NetworkCtx {
            ap_ssid: Some("ADOS-Setup-9F2C".to_string()),
            ap_ip: Some("10.42.0.1".to_string()),
            usb_ip: None,
            uplink_type: Some("none".to_string()),
            uplink_reachable: false,
            mdns_host: Some("ados-9f2c1a".to_string()),
            hotspot_ssid: Some("ADOS-Setup-9F2C".to_string()),
            hotspot_enabled: true,
            wifi_client: WifiClientCtx {
                connected: false,
                ssid: None,
                signal_dbm: None,
            },
        },
        uplink: UplinkCtx {
            modem_present: false,
            reason: Some("No modem detected".to_string()),
            ..UplinkCtx::default()
        },
        system: SystemCtx {
            cpu_pct: Some(12.0),
            ram_used_mb: Some(410.0),
            ram_total_mb: Some(3840.0),
            temp_c: Some(43.0),
            uptime_seconds: Some(92.0),
            agent_version: Some("0.49.41".to_string()),
            cpu_history: Vec::new(),
            temp_history: Vec::new(),
        },
        hardware_check: vec![
            HardwareItem {
                id: Some("radio".to_string()),
                label: Some("WFB radio".to_string()),
                state: Some("missing".to_string()),
                fix_hint: Some("Plug in the RTL8812EU dongle".to_string()),
            },
            HardwareItem {
                id: Some("display".to_string()),
                label: Some("LCD panel".to_string()),
                state: Some("ok".to_string()),
                fix_hint: None,
            },
            HardwareItem {
                id: Some("fc".to_string()),
                label: Some("Flight controller".to_string()),
                state: Some("unknown".to_string()),
                fix_hint: None,
            },
        ],
        hopping: HoppingCtx {
            band: Some("u-nii-3".to_string()),
            radio_channel: None,
            history: Vec::new(),
        },
        video: VideoCtx {
            decoder: None,
            active: false,
            recording: false,
            fps: None,
            latency_ms: None,
            bitrate_kbps: None,
            mediamtx_ready: false,
            mediamtx_inbound_kbps: None,
            camera_label: None,
            camera_count: 0,
        },
        health: HealthCtx {
            cpu_percent: Some(12.0),
            memory_percent: Some(10.7),
            disk_percent: Some(31.0),
            temperature: Some(43.0),
        },
        device: DeviceCtx {
            device_id: Some("ados-9f2c1a40".to_string()),
            device_name: Some("ADOS Node".to_string()),
            version: Some("0.49.41".to_string()),
            board_name: Some("Reference SBC".to_string()),
            mac_eth0: Some("DC:A6:32:1A:2B:3C".to_string()),
            mac_wlan0: Some("DC:A6:32:1A:2B:3D".to_string()),
            primary_ip: Some("192.168.200.115".to_string()),
            primary_mac: Some("DC:A6:32:1A:2B:3C".to_string()),
            build_stamp: Some("2026-05-31T22:14:00Z".to_string()),
        },
        settings: SettingsCtx {
            rows: vec![
                SettingsRow {
                    id: "theme".to_string(),
                    label: "Theme".to_string(),
                    variant: "default".to_string(),
                    value: Some("Dark".to_string()),
                    toggle_on: None,
                },
                SettingsRow {
                    id: "hotspot".to_string(),
                    label: "WiFi hotspot".to_string(),
                    variant: "toggle".to_string(),
                    value: None,
                    toggle_on: Some(true),
                },
                SettingsRow {
                    id: "logging".to_string(),
                    label: "Log level".to_string(),
                    variant: "default".to_string(),
                    value: Some("DEBUG".to_string()),
                    toggle_on: None,
                },
            ],
            pending_reboot_count: 0,
            theme: Some("dark".to_string()),
            logging_level: Some("DEBUG".to_string()),
            display_rotation_degrees: Some(0),
            server_mode: Some("local".to_string()),
        },
        diagnostics: DiagnosticsCtx {
            agent_logs: vec![
                "INFO  bootstrap: first boot, profile=ground_station".to_string(),
                "WARN  radio: no WFB adapter present".to_string(),
                "INFO  setup: captive portal up on 10.42.0.1".to_string(),
                "INFO  cloud: broadcasting pair code 4KD9TZ".to_string(),
            ],
            log_scroll_offset: 0,
        },
    }
}

/// One renderable page paired with the stable id used in the output filename.
struct PageEntry {
    file_id: &'static str,
    page: Box<dyn Page>,
}

/// Every page the LCD UI ships, in the same registration order the daemon uses.
fn all_pages() -> Vec<PageEntry> {
    vec![
        PageEntry {
            file_id: "dashboard",
            page: Box::new(DashboardPage),
        },
        PageEntry {
            file_id: "video",
            page: Box::new(VideoPage),
        },
        PageEntry {
            file_id: "settings",
            page: Box::new(SettingsPage),
        },
        PageEntry {
            file_id: "link_stats",
            page: Box::new(LinkStatsPage),
        },
        PageEntry {
            file_id: "channel_hops",
            page: Box::new(ChannelHopsPage),
        },
        PageEntry {
            file_id: "more",
            page: Box::new(MorePage),
        },
        PageEntry {
            file_id: "radio_link",
            page: Box::new(RadioLinkDetailPage::new()),
        },
        PageEntry {
            file_id: "uplink",
            page: Box::new(UplinkDetailPage),
        },
        PageEntry {
            file_id: "drone",
            page: Box::new(DroneDetailPage),
        },
        PageEntry {
            file_id: "mesh",
            page: Box::new(MeshDetailPage::new()),
        },
        PageEntry {
            file_id: "about",
            page: Box::new(AboutDetailPage),
        },
        PageEntry {
            file_id: "pair_drone",
            page: Box::new(PairDroneDetailPage),
        },
        PageEntry {
            file_id: "diagnostics",
            page: Box::new(DiagnosticsDetailPage),
        },
    ]
}

/// Write a finished page canvas to `path` as an RGB PNG. The canvas already
/// holds tightly-packed RGB888 in the panel's native 480x320 geometry.
fn write_png(canvas: &Canvas, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let buffer: image::RgbImage =
        image::ImageBuffer::from_raw(canvas.width(), canvas.height(), canvas.as_rgb888().to_vec())
            .ok_or("canvas RGB888 buffer did not match its declared geometry")?;
    buffer.save(path)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent (the crates workspace dir)")
        .join("target")
        .join("ui-parity");
    std::fs::create_dir_all(&out_dir)?;

    let palette: &Palette = &DARK;
    let variants: [(&str, PageContext); 2] = [
        ("connected", connected_context()),
        ("unpaired", unpaired_context()),
    ];

    let pages = all_pages();
    let mut written: Vec<String> = Vec::new();

    for (variant, ctx) in &variants {
        for entry in &pages {
            let canvas = entry.page.render(ctx, palette);
            assert_eq!(
                canvas.width(),
                ados_display::pages::PANEL_W,
                "{} ({variant}) rendered the wrong width",
                entry.file_id
            );
            assert_eq!(
                canvas.height(),
                ados_display::pages::PANEL_H,
                "{} ({variant}) rendered the wrong height",
                entry.file_id
            );
            let name = format!("{}_{variant}.png", entry.file_id);
            let path = out_dir.join(&name);
            write_png(&canvas, &path)?;
            written.push(path.display().to_string());
        }
    }

    println!(
        "rendered {} page frames to {}",
        written.len(),
        out_dir.display()
    );
    for p in &written {
        println!("  {p}");
    }
    Ok(())
}
