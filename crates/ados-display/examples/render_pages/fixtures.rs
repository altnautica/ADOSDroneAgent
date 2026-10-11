//! Synthetic [`PageContext`] fixtures for the LCD page renders. Every
//! hostname, id, address and MAC here is made up: IPs are documentation-style
//! LAN examples and the setup / USB gadget defaults, MACs are locally
//! administered.

use ados_display::pages::{
    CloudCtx, DeviceCtx, DiagnosticsCtx, DroneCtx, FcCtx, HardwareItem, HopEntry, HoppingCtx,
    LinkCtx, MeshCtx, MeshPeer, NetworkCtx, PageContext, PairedDroneCtx, PairingCtx, RoleCtx,
    SystemCtx, UplinkCtx, VideoCtx, WifiClientCtx,
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
pub fn connected_context() -> PageContext {
    PageContext {
        lan_host: Some("ados-9f2c1a.local".to_string()),
        mission_control_url: None,
        hostname: "ados-58c27f".to_string(),
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
            bitrate_kbps: Some(18_400.0),
            fec_recovered: Some(214),
            fec_lost: Some(3),
            channel: Some(149),
            frequency_mhz: Some(5745),
            bandwidth_mhz: Some(20),
            tx_power_dbm: Some(22),
            topology: Some("external_5v".to_string()),
            mcs_index: Some(5),
            fec_k: Some(8),
            fec_n: Some(10),
            adaptive_bitrate_enabled: Some(true),
            recommended_tier_name: Some("high".to_string()),
            packets_received: Some(1_482_910),
            packets_lost: Some(1_204),
            rssi_history: wave(-58.0, 6.0),
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
            auto_pair_enabled: Some(true),
        },
        fc: FcCtx {
            vehicle: Some("Multirotor".to_string()),
            mode: Some("LOITER".to_string()),
            armed: Some(false),
            battery_voltage: Some(15.7),
            battery_remaining: Some(78.0),
            gps_fix_type: Some(4),
            gps_satellites_visible: Some(17),
            battery_history: wave(78.0, 5.0),
        },
        cloud: CloudCtx {
            paired: true,
            pair_code: Some("7QX4M2".to_string()),
            latency_ms: Some(48.0),
            rtt_ms: Some(96.0),
            broadcasting: true,
            mqtt_state: Some("connected".to_string()),
            http_state: Some("ok".to_string()),
            drone_id: Some("ados-58c27faf".to_string()),
        },
        pairing: PairingCtx {
            code: Some("7QX4M2".to_string()),
            window_active: false,
            window_remaining_seconds: None,
        },
        role: RoleCtx {
            current: Some("relay".to_string()),
            configured: Some("relay".to_string()),
            mesh_capable: true,
        },
        mesh: MeshCtx {
            up: Some(true),
            stale: false,
            partition: false,
            peer_count: Some(2),
            selected_gateway: Some("ados-aa11bb22".to_string()),
            mesh_id: Some("alt-mesh-01".to_string()),
            peers: Some(vec![
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
            ]),
        },
        network: NetworkCtx {
            ap_passphrase: Some("EXAMPLEPASS99".into()),
            ap_ssid: Some("ADOS-GS-9F2C".to_string()),
            ap_ip: Some("192.168.4.1".to_string()),
            usb_ip: Some("192.168.7.1".to_string()),
            uplink_type: Some("cellular".to_string()),
            uplink_reachable: Some(true),
            hotspot_ssid: Some("ADOS-GS-9F2C".to_string()),
            wifi_client: WifiClientCtx {
                connected: true,
                ssid: Some("HomeNetwork".to_string()),
                signal_dbm: Some(-52.0),
            },
        },
        uplink: UplinkCtx {
            modem_present: true,
            rsrp_dbm: Some(-84.0),
            rsrq_db: Some(-11.0),
            sinr_db: Some(14.0),
            band: Some("B40".to_string()),
            ip: Some("192.168.1.60".to_string()),
            tech: Some("LTE".to_string()),
            reason: None,
        },
        system: SystemCtx {
            cpu_pct: Some(31.0),
            ram_used_mb: Some(742.0),
            ram_total_mb: Some(3840.0),
            temp_c: Some(52.0),
            disk_pct: Some(44.0),
            uptime_seconds: Some(7384.0),
            agent_version: Some("0.49.41".to_string()),
            cpu_history: wave(31.0, 18.0),
            temp_history: wave(52.0, 6.0),
        },
        hardware_check: vec![
            HardwareItem {
                id: Some("radio_wfb".to_string()),
                required: true,
                label: Some("WFB radio".to_string()),
                state: Some("ok".to_string()),
                fix_hint: None,
            },
            HardwareItem {
                id: Some("display".to_string()),
                required: false,
                label: Some("LCD panel".to_string()),
                state: Some("ok".to_string()),
                fix_hint: None,
            },
            HardwareItem {
                id: Some("uplink".to_string()),
                required: false,
                label: Some("Cellular modem".to_string()),
                state: Some("warning".to_string()),
                fix_hint: Some("Weak signal, reposition antenna".to_string()),
            },
        ],
        hopping: HoppingCtx {
            present: true,
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
            active: Some(true),
            recording: true,
            fps: Some(48.0),
            latency_ms: Some(62.0),
            bitrate_kbps: Some(4_180.0),
            mediamtx_ready: Some(true),
            mediamtx_inbound_kbps: Some(4_096.0),
            camera_label: Some("USB UVC".to_string()),
            camera_count: 1,
        },
        device: DeviceCtx {
            device_id: Some("ados-9f2c1a40".to_string()),
            device_name: Some("Ground Node".to_string()),
            version: Some("0.49.41".to_string()),
            board_name: Some("Reference SBC".to_string()),
            mac_wired: Some("02:A0:5D:1A:2B:3C".to_string()),
            mac_wireless: Some("02:A0:5D:1A:2B:3D".to_string()),
            primary_ip: Some("192.168.1.50".to_string()),
            primary_mac: Some("02:A0:5D:1A:2B:3C".to_string()),
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
        },
    }
}

/// A fresh-boot, unpaired ground station: no link, no drone, the setup wizard
/// still open, an active local pairing window, and the cloud relay broadcasting
/// a pair code. Exercises every page's empty / waiting / unpaired branch.
pub fn unpaired_context() -> PageContext {
    PageContext {
        lan_host: Some("ados-9f2c1a.local".to_string()),
        mission_control_url: None,
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
        drone: DroneCtx::default(),
        paired_drone: PairedDroneCtx::default(),
        fc: FcCtx {
            battery_history: Vec::new(),
            ..FcCtx::default()
        },
        cloud: CloudCtx {
            paired: false,
            pair_code: Some("4KD9TZ".to_string()),
            broadcasting: true,
            mqtt_state: Some("connecting".to_string()),
            http_state: Some("connecting".to_string()),
            ..CloudCtx::default()
        },
        pairing: PairingCtx {
            code: Some("4KD9TZ".to_string()),
            window_active: true,
            window_remaining_seconds: Some(118.0),
        },
        role: RoleCtx {
            current: Some("direct".to_string()),
            configured: Some("direct".to_string()),
            mesh_capable: false,
        },
        mesh: MeshCtx {
            up: Some(false),
            stale: false,
            partition: false,
            peer_count: Some(0),
            selected_gateway: None,
            mesh_id: None,
            peers: Some(Vec::new()),
        },
        network: NetworkCtx {
            ap_passphrase: Some("EXAMPLEPASS99".into()),
            ap_ssid: Some("ADOS-Setup-9F2C".to_string()),
            ap_ip: Some("192.168.4.1".to_string()),
            usb_ip: None,
            uplink_type: Some("none".to_string()),
            uplink_reachable: Some(false),
            hotspot_ssid: Some("ADOS-Setup-9F2C".to_string()),
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
            disk_pct: Some(31.0),
            uptime_seconds: Some(92.0),
            agent_version: Some("0.49.41".to_string()),
            cpu_history: Vec::new(),
            temp_history: Vec::new(),
        },
        hardware_check: vec![
            HardwareItem {
                id: Some("radio_wfb".to_string()),
                required: true,
                label: Some("WFB radio".to_string()),
                state: Some("missing".to_string()),
                fix_hint: Some("Plug in the RTL8812EU dongle".to_string()),
            },
            HardwareItem {
                id: Some("display".to_string()),
                required: false,
                label: Some("LCD panel".to_string()),
                state: Some("ok".to_string()),
                fix_hint: None,
            },
            HardwareItem {
                id: Some("fc".to_string()),
                required: false,
                label: Some("Flight controller".to_string()),
                state: Some("unknown".to_string()),
                fix_hint: None,
            },
        ],
        hopping: HoppingCtx {
            present: true,
            band: Some("u-nii-3".to_string()),
            radio_channel: None,
            history: Vec::new(),
        },
        video: VideoCtx {
            decoder: None,
            active: Some(false),
            recording: false,
            fps: None,
            latency_ms: None,
            bitrate_kbps: None,
            mediamtx_ready: Some(false),
            mediamtx_inbound_kbps: None,
            camera_label: None,
            camera_count: 0,
        },
        device: DeviceCtx {
            device_id: Some("ados-9f2c1a40".to_string()),
            device_name: Some("ADOS Node".to_string()),
            version: Some("0.49.41".to_string()),
            board_name: Some("Reference SBC".to_string()),
            mac_wired: Some("02:A0:5D:1A:2B:3C".to_string()),
            mac_wireless: Some("02:A0:5D:1A:2B:3D".to_string()),
            primary_ip: Some("192.168.1.51".to_string()),
            primary_mac: Some("02:A0:5D:1A:2B:3C".to_string()),
        },
        diagnostics: DiagnosticsCtx {
            agent_logs: vec![
                "INFO  bootstrap: first boot, profile=ground_station".to_string(),
                "WARN  radio: no WFB adapter present".to_string(),
                "INFO  setup: captive portal up on 192.168.4.1".to_string(),
                "INFO  cloud: broadcasting pair code 4KD9TZ".to_string(),
            ],
        },
    }
}

/// The connected ground station acting as the mesh receiver: the gateway of a
/// three-relay mesh, so the Mesh page shows the RECEIVER badge and the relays.
pub fn receiver_context() -> PageContext {
    let mut ctx = connected_context();
    ctx.hostname = "ados-9f2c1a".to_string();
    ctx.role = RoleCtx {
        current: Some("receiver".to_string()),
        configured: Some("receiver".to_string()),
        mesh_capable: true,
    };
    ctx.mesh = MeshCtx {
        up: Some(true),
        stale: false,
        partition: false,
        peer_count: Some(3),
        selected_gateway: Some("ados-9f2c1a40".to_string()),
        mesh_id: Some("alt-mesh-01".to_string()),
        peers: Some(
            [
                ("ados-aa11bb22", 1.0),
                ("ados-cc33dd44", 3.0),
                ("ados-ee55ff66", 6.0),
            ]
            .into_iter()
            .map(|(id, seen)| MeshPeer {
                device_id: Some(id.to_string()),
                role: Some("relay".to_string()),
                last_seen_seconds_ago: Some(seen),
            })
            .collect(),
        ),
    };
    ctx
}
