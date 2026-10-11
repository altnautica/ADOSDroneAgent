use super::*;

#[test]
fn missing_file_yields_python_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let c = WfbConfig::load_from(&dir.path().join("nope.yaml"));
    assert_eq!(c.channel, 149);
    assert_eq!(c.band, "u-nii-3");
    assert!(c.auto_hop_enabled);
    assert_eq!(c.hop_period_seconds, 60);
    assert!((c.hop_loss_threshold_percent - 10.0).abs() < 0.01);
    assert!((c.hop_rssi_threshold_dbm - (-75.0)).abs() < 0.01);
    assert_eq!(c.fec_k, 8);
    assert_eq!(c.fec_n, 12);
    // An absent `backend` key resolves to the auto default (kernel on Linux).
    assert_eq!(c.backend, BackendChoice::Auto);
}

#[test]
fn backend_choice_reads_and_defaults_permissively() {
    let dir = tempfile::tempdir().unwrap();
    // Missing file → the auto default.
    let c = WfbConfig::load_from(&dir.path().join("nope.yaml"));
    assert_eq!(c.backend, BackendChoice::Auto);
    // Explicit kernel / userspace parse.
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "video:\n  wfb:\n    backend: kernel\n").unwrap();
    assert_eq!(WfbConfig::load_from(&cfg).backend, BackendChoice::Kernel);
    std::fs::write(&cfg, "video:\n  wfb:\n    backend: userspace\n").unwrap();
    assert_eq!(WfbConfig::load_from(&cfg).backend, BackendChoice::Userspace);
    // An unknown value is permissive → auto, not a config-load failure.
    std::fs::write(&cfg, "video:\n  wfb:\n    backend: bogus\n").unwrap();
    assert_eq!(WfbConfig::load_from(&cfg).backend, BackendChoice::Auto);
    // Case-insensitive.
    assert_eq!(BackendChoice::from_wire("KERNEL"), BackendChoice::Kernel);
    assert_eq!(BackendChoice::from_wire(""), BackendChoice::Auto);
}

#[test]
fn profile_gate_detects_ground_station_and_drone() {
    let dir = tempfile::tempdir().unwrap();
    let none = dir.path().join("nope.yaml");
    let none2 = dir.path().join("nope.conf");
    // Missing everything → not GS (default drone).
    assert!(!profile_is_ground_station(&none, &none2));
    // Explicit GS in config.yaml.
    let gs = dir.path().join("gs.yaml");
    std::fs::write(&gs, "agent:\n  profile: ground_station\n").unwrap();
    assert!(profile_is_ground_station(&gs, &none2));
    // Explicit drone in config.yaml.
    let dr = dir.path().join("dr.yaml");
    std::fs::write(&dr, "agent:\n  profile: drone\n").unwrap();
    assert!(!profile_is_ground_station(&dr, &none2));
    // auto in config.yaml → consult profile.conf (GS).
    let auto = dir.path().join("auto.yaml");
    std::fs::write(&auto, "agent:\n  profile: auto\n").unwrap();
    let pc = dir.path().join("profile.conf");
    std::fs::write(&pc, "profile: ground-station\n").unwrap();
    assert!(profile_is_ground_station(&auto, &pc));
}

#[test]
fn reads_wfb_section() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    channel: 36\n    band: u-nii-1\n    auto_hop_enabled: false\n",
    )
    .unwrap();
    let c = WfbConfig::load_from(&cfg);
    assert_eq!(c.channel, 36);
    assert_eq!(c.band, "u-nii-1");
    assert!(!c.auto_hop_enabled);
    // Unset fields fall back to defaults.
    assert_eq!(c.mcs_index, 1);
    // Pair fields absent → None.
    assert!(c.paired_with_device_id.is_none());
    assert!(c.paired_at.is_none());
}

#[test]
fn reads_pair_block_from_wfb_section() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    paired_with_device_id: ados-58c27faf\n    paired_at: '2026-05-31T10:06:00+00:00'\n    auto_pair_enabled: false\n",
    )
    .unwrap();
    let c = WfbConfig::load_from(&cfg);
    assert_eq!(c.paired_with_device_id.as_deref(), Some("ados-58c27faf"));
    assert_eq!(c.paired_at.as_deref(), Some("2026-05-31T10:06:00+00:00"));
    assert!(!c.auto_pair_enabled);
}

#[test]
fn link_preset_trio_maps_each_named_preset() {
    // The trio must be byte-identical to the Python _LINK_PRESETS table.
    assert_eq!(link_preset_trio("conservative"), Some((1, 8, 12)));
    assert_eq!(link_preset_trio("balanced"), Some((3, 8, 12)));
    assert_eq!(link_preset_trio("aggressive"), Some((5, 8, 10)));
    // An unknown name yields None so the caller keeps the current config.
    assert_eq!(link_preset_trio("turbo"), None);
    assert_eq!(link_preset_trio(""), None);
}

#[test]
fn apply_preset_conservative_is_noop() {
    // conservative respects whatever was explicitly configured, even when
    // those values differ from the conservative trio.
    let mut c = WfbConfig {
        wfb_link_preset: "conservative".to_string(),
        mcs_index: 4,
        fec_k: 6,
        fec_n: 9,
        ..WfbConfig::default()
    };
    c.apply_link_preset();
    assert_eq!(c.mcs_index, 4);
    assert_eq!(c.fec_k, 6);
    assert_eq!(c.fec_n, 9);
}

#[test]
fn apply_preset_balanced_forces_trio() {
    let mut c = WfbConfig {
        wfb_link_preset: "balanced".to_string(),
        mcs_index: 1,
        fec_k: 8,
        fec_n: 12,
        ..WfbConfig::default()
    };
    c.apply_link_preset();
    assert_eq!(c.mcs_index, 3);
    assert_eq!(c.fec_k, 8);
    assert_eq!(c.fec_n, 12);
}

#[test]
fn apply_preset_aggressive_forces_trio() {
    let mut c = WfbConfig {
        wfb_link_preset: "aggressive".to_string(),
        ..WfbConfig::default()
    };
    c.apply_link_preset();
    assert_eq!(c.mcs_index, 5);
    assert_eq!(c.fec_k, 8);
    assert_eq!(c.fec_n, 10);
}

#[test]
fn apply_preset_unknown_keeps_current_values() {
    let mut c = WfbConfig {
        wfb_link_preset: "turbo".to_string(),
        mcs_index: 2,
        fec_k: 7,
        fec_n: 11,
        ..WfbConfig::default()
    };
    c.apply_link_preset();
    // Unknown preset → no override; the configured values stand.
    assert_eq!(c.mcs_index, 2);
    assert_eq!(c.fec_k, 7);
    assert_eq!(c.fec_n, 11);
}

#[test]
fn link_preset_defaults_to_conservative() {
    let c = WfbConfig::default();
    assert_eq!(c.wfb_link_preset, "conservative");
    // The adaptive FEC controller is armed by default.
    assert!(c.adaptive_bitrate_enabled);
}

#[test]
fn reg_gate_raw_flag_default_is_retained_for_region_pin() {
    // The raw `reg_gate_strict` flag default stays `true` so a pinned region
    // is byte-identical to the legacy fail-closed behaviour. The operating
    // mode (RegulatoryConfig) is what gates whether that flag is in force, not
    // a change to this default.
    let c = WfbConfig::default();
    assert!(c.reg_gate_strict);
    // DFS home is off unless explicitly opted in.
    assert!(!c.dfs_allowed);
    // No rendezvous pin → rendezvous == home channel.
    assert!(c.rendezvous_channel.is_none());
    assert_eq!(c.rendezvous_channel(), c.channel);
}

#[test]
fn regulatory_default_is_unrestricted_with_no_region() {
    // The fresh-box operating posture: a default RegulatoryConfig is
    // unrestricted with no pinned region. This is what makes the EFFECTIVE
    // gate permissive even though the raw `reg_gate_strict` flag stays true.
    let r = RegulatoryConfig::default();
    assert_eq!(r.mode, RegulatoryMode::Unrestricted);
    assert!(r.mode.is_unrestricted());
    assert!(r.region.is_none());
    assert_eq!(r.mode.as_wire(), "unrestricted");
}

#[test]
fn regulatory_missing_file_or_block_reads_unrestricted() {
    let dir = tempfile::tempdir().unwrap();
    // No file at all.
    let r = RegulatoryConfig::load_from(&dir.path().join("nope.yaml"));
    assert_eq!(r.mode, RegulatoryMode::Unrestricted);
    assert!(r.region.is_none());
    // A config with no network.regulatory block.
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "video:\n  wfb:\n    channel: 149\n").unwrap();
    let r = RegulatoryConfig::load_from(&cfg);
    assert_eq!(r.mode, RegulatoryMode::Unrestricted);
    assert!(r.region.is_none());
}

#[test]
fn regulatory_region_mode_reads_and_uppercases_region() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "network:\n  regulatory:\n    mode: region\n    region: in\n    ack_operator: ada\n    ack_at: '2026-06-03T00:00:00+05:30'\n",
    )
    .unwrap();
    let r = RegulatoryConfig::load_from(&cfg);
    assert_eq!(r.mode, RegulatoryMode::Region);
    assert_eq!(r.region.as_deref(), Some("IN"));
    assert_eq!(r.ack_operator.as_deref(), Some("ada"));
    assert_eq!(r.ack_at.as_deref(), Some("2026-06-03T00:00:00+05:30"));
}

#[test]
fn regulatory_region_mode_without_code_degrades_to_unrestricted() {
    // A `region` mode with no region code has no jurisdiction to enforce, so
    // it degrades to unrestricted (the safe-permissive direction), never
    // fail-closed.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "network:\n  regulatory:\n    mode: region\n").unwrap();
    let r = RegulatoryConfig::load_from(&cfg);
    assert_eq!(r.mode, RegulatoryMode::Unrestricted);
    assert!(r.region.is_none());
}

#[test]
fn regulatory_explicit_unrestricted_clears_any_region() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "network:\n  regulatory:\n    mode: unrestricted\n    region: US\n",
    )
    .unwrap();
    let r = RegulatoryConfig::load_from(&cfg);
    assert_eq!(r.mode, RegulatoryMode::Unrestricted);
    // The region is irrelevant under unrestricted; it is dropped.
    assert!(r.region.is_none());
}

#[test]
fn regulatory_wanted_domain_resolution() {
    // Region → the pinned region code.
    let region = RegulatoryConfig {
        mode: RegulatoryMode::Region,
        region: Some("DE".to_string()),
        ..RegulatoryConfig::default()
    };
    assert_eq!(region.wanted_domain("US"), "DE");
    // Unrestricted → the configured fallback (so the global reconciler keeps a
    // sane domain, never the world default).
    let unrestricted = RegulatoryConfig::default();
    assert_eq!(unrestricted.wanted_domain("US"), "US");
    assert_eq!(unrestricted.wanted_domain("GB"), "GB");
}

#[test]
fn regulatory_mode_from_wire_is_permissive() {
    assert_eq!(RegulatoryMode::from_wire("region"), RegulatoryMode::Region);
    assert_eq!(RegulatoryMode::from_wire("REGION"), RegulatoryMode::Region);
    assert_eq!(
        RegulatoryMode::from_wire("unrestricted"),
        RegulatoryMode::Unrestricted
    );
    // Anything unrecognised reads as unrestricted (permissive default).
    assert_eq!(RegulatoryMode::from_wire(""), RegulatoryMode::Unrestricted);
    assert_eq!(
        RegulatoryMode::from_wire("garbage"),
        RegulatoryMode::Unrestricted
    );
}

#[test]
fn reg_gate_keys_read_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    channel: 149\n    reg_gate_strict: false\n    dfs_allowed: true\n    rendezvous_channel: 153\n",
    )
    .unwrap();
    let c = WfbConfig::load_from(&cfg);
    assert!(!c.reg_gate_strict);
    assert!(c.dfs_allowed);
    assert_eq!(c.rendezvous_channel, Some(153));
    // The pin overrides the home channel for rendezvous.
    assert_eq!(c.rendezvous_channel(), 153);
}

#[test]
fn reg_gate_keys_absent_fall_back_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // A config that sets only the home channel: the new keys take defaults.
    std::fs::write(&cfg, "video:\n  wfb:\n    channel: 149\n").unwrap();
    let c = WfbConfig::load_from(&cfg);
    assert!(c.reg_gate_strict);
    assert!(!c.dfs_allowed);
    assert_eq!(c.rendezvous_channel(), 149);
}

#[test]
fn aux_stream_defaults_are_permitted_and_clear_of_other_planes() {
    // The aux allow flag is ON so a linked ground station can receive
    // capability over the radio, but permitting is not starting: nothing
    // spawns and nothing radiates until a consumer explicitly opens the
    // stream (the process layer enforces that). The aux ports must never
    // collide with the data (5600) or control (5803/5810) ports.
    let c = WfbConfig::default();
    assert!(c.aux_enable);
    assert_eq!(c.aux_tx_port, 5602);
    assert_eq!(c.aux_rx_port, 5603);
    // The radio's defaults and the shared reader's must be the same pair:
    // the router and the control surface resolve through the reader, and a
    // drift between them takes the uplink down with no error anywhere.
    let shared = ados_protocol::aux_ports::AuxPorts::default();
    assert_eq!(c.aux_tx_port, shared.tx);
    assert_eq!(c.aux_rx_port, shared.rx);
    assert_eq!(c.aux_fec_k, 1);
    assert_eq!(c.aux_fec_n, 2);
    // No explicit aux MCS → the aux pair rides the data-plane rate.
    assert!(c.aux_mcs_index.is_none());
    assert_eq!(c.aux_mcs(), c.mcs_index);
    // The aux ports are distinct from the data/control/stats ports.
    for reserved in [5600u16, 5601, 5803, 5810] {
        assert_ne!(c.aux_tx_port, reserved);
        assert_ne!(c.aux_rx_port, reserved);
    }
}

#[test]
fn aux_port_collision_detects_reserved_and_self_collisions() {
    // The safe default ports are clear.
    assert_eq!(aux_port_collision(5602, 5603), None);
    // An aux tx reusing the data-plane ingress (5600) collides.
    assert_eq!(
        aux_port_collision(5600, 5603),
        Some(AuxPortCollision::TxReservesPlanePort(5600))
    );
    // An aux rx reusing the control rx (5810) collides.
    assert_eq!(
        aux_port_collision(5602, 5810),
        Some(AuxPortCollision::RxReservesPlanePort(5810))
    );
    // Stats (5601) and control tx (5803) are reserved too.
    assert_eq!(
        aux_port_collision(5601, 5603),
        Some(AuxPortCollision::TxReservesPlanePort(5601))
    );
    assert_eq!(
        aux_port_collision(5602, 5803),
        Some(AuxPortCollision::RxReservesPlanePort(5803))
    );
    // Aux tx == aux rx (receive would feed transmit).
    assert_eq!(
        aux_port_collision(5612, 5612),
        Some(AuxPortCollision::TxRxSame(5612))
    );
}

#[test]
fn loading_a_colliding_aux_port_disables_the_aux_stream() {
    // An operator that sets aux_tx_port onto the data plane (5600) must not
    // silently collide: the load-time guard disables the aux stream.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    aux_enable: true\n    aux_tx_port: 5600\n    aux_rx_port: 5603\n",
    )
    .unwrap();
    let c = WfbConfig::load_from(&cfg);
    // The guard fired: the aux stream is disabled despite aux_enable: true.
    assert!(!c.aux_enable);

    // A clear configuration keeps the aux stream enabled.
    let ok = dir.path().join("ok.yaml");
    std::fs::write(
        &ok,
        "video:\n  wfb:\n    aux_enable: true\n    aux_tx_port: 5612\n    aux_rx_port: 5613\n",
    )
    .unwrap();
    let c2 = WfbConfig::load_from(&ok);
    assert!(c2.aux_enable);
    assert_eq!(c2.aux_tx_port, 5612);
    assert_eq!(c2.aux_rx_port, 5613);
}

#[test]
fn aux_stream_keys_read_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    aux_enable: true\n    aux_tx_port: 5612\n    aux_rx_port: 5613\n    aux_fec_k: 4\n    aux_fec_n: 8\n    aux_mcs_index: 3\n",
    )
    .unwrap();
    let c = WfbConfig::load_from(&cfg);
    assert!(c.aux_enable);
    assert_eq!(c.aux_tx_port, 5612);
    assert_eq!(c.aux_rx_port, 5613);
    assert_eq!(c.aux_fec_k, 4);
    assert_eq!(c.aux_fec_n, 8);
    // The explicit aux MCS overrides the data-plane rate.
    assert_eq!(c.aux_mcs_index, Some(3));
    assert_eq!(c.aux_mcs(), 3);
}

#[test]
fn fleet_defaults_are_fleet_one_ground_slot() {
    let c = WfbConfig::default();
    assert_eq!(c.fleet_id, 1);
    assert_eq!(c.fleet_slot, SLOT_GROUND);
}

#[test]
fn fleet_identity_reads_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    fleet_id: 7\n    fleet_slot: 12\n",
    )
    .unwrap();
    let c = WfbConfig::load_from(&cfg);
    assert_eq!(c.fleet_id, 7);
    assert_eq!(c.fleet_slot, 12);
}

#[test]
fn link_id_is_unique_per_slot_and_never_collides_across_radio_ports() {
    // Two transmitters sharing a channel_id thrash each other's FEC decoder,
    // so every (slot, radio_port) pair in a fleet must map to a distinct
    // channel_id. channel_id = (link_id << 8) + radio_port.
    let mut seen = std::collections::BTreeSet::new();
    for slot in 0..=FLEET_MAX_SLOTS {
        for radio_port in 0u32..4 {
            let channel_id = (link_id(1, slot) << 8) + radio_port;
            assert!(
                seen.insert(channel_id),
                "duplicate channel_id for slot {slot} port {radio_port}"
            );
        }
    }
    assert_ne!(link_id(1, SLOT_GROUND), link_id(1, 1));
}

#[test]
fn link_id_separates_fleets_sharing_a_channel() {
    // 16 bits of fleet over 8 bits of slot fills the 24-bit link_id space
    // exactly, so no slot in fleet A can alias a slot in fleet B.
    assert_eq!(link_id(1, 0), 0x000100);
    assert_eq!(link_id(1, 24), 0x000118);
    assert_eq!(link_id(2, 0), 0x000200);
    assert_ne!(link_id(1, 255), link_id(2, 0));
    assert_eq!(link_id(u16::MAX, u8::MAX), 0x00FF_FFFF);
}

#[test]
fn fleet_identity_rejects_unprovisioned_and_out_of_range() {
    assert_eq!(
        fleet_identity_error(0, 1, false),
        Some(FleetIdentityError::UnprovisionedFleet)
    );
    assert_eq!(
        fleet_identity_error(1, FLEET_MAX_SLOTS + 1, false),
        Some(FleetIdentityError::SlotOutOfRange(FLEET_MAX_SLOTS + 1))
    );
}

#[test]
fn fleet_identity_is_profile_aware() {
    // A drone with the ground station's slot would key its transmitters onto
    // the uplink channel_id — the FEC-thrash failure mode.
    assert_eq!(
        fleet_identity_error(1, SLOT_GROUND, false),
        Some(FleetIdentityError::DroneWithoutSlot)
    );
    assert_eq!(
        fleet_identity_error(1, 3, true),
        Some(FleetIdentityError::GroundWithDroneSlot(3))
    );
    assert_eq!(fleet_identity_error(1, 1, false), None);
    assert_eq!(fleet_identity_error(1, FLEET_MAX_SLOTS, false), None);
    assert_eq!(fleet_identity_error(1, SLOT_GROUND, true), None);
}

#[test]
fn aux_ports_may_not_land_in_the_fleet_receiver_span() {
    // A fleet receiver decoding into the aux transmit ingress would radiate
    // every decoded downlink frame straight back over the air.
    assert_eq!(
        aux_port_collision(VIDEO_RX_PORT_BASE + 1, 5613),
        Some(AuxPortCollision::TxReservesFleetRxPort(
            VIDEO_RX_PORT_BASE + 1
        ))
    );
    assert_eq!(
        aux_port_collision(5612, CONTROL_RX_PORT_BASE + FLEET_MAX_SLOTS as u16),
        Some(AuxPortCollision::RxReservesFleetRxPort(
            CONTROL_RX_PORT_BASE + FLEET_MAX_SLOTS as u16
        ))
    );
    // The span starts above the fixed plane ports, so the shipped defaults
    // (drone aux 5602/5603, ground uplink 5602/5810) stay clear of it.
    assert!(!is_fleet_rx_port(VIDEO_RX_PORT_BASE));
    assert!(!is_fleet_rx_port(default_aux_tx_port()));
    assert!(!is_fleet_rx_port(default_aux_rx_port()));
    assert!(!is_fleet_rx_port(5810));
    assert_eq!(
        aux_port_collision(default_aux_tx_port(), default_aux_rx_port()),
        None
    );
}

#[test]
fn fleet_receiver_ranges_do_not_overlap_each_other() {
    let video: Vec<u16> = (1..=FLEET_MAX_SLOTS)
        .map(|s| VIDEO_RX_PORT_BASE + s as u16)
        .collect();
    let aux: Vec<u16> = (1..=FLEET_MAX_SLOTS)
        .map(|s| AUX_RX_PORT_BASE + s as u16)
        .collect();
    let control: Vec<u16> = (1..=FLEET_MAX_SLOTS)
        .map(|s| CONTROL_RX_PORT_BASE + s as u16)
        .collect();
    let mut all: Vec<u16> = video
        .iter()
        .chain(aux.iter())
        .chain(control.iter())
        .copied()
        .collect();
    let total = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), total, "per-slot receiver ports must be distinct");
    for p in &all {
        assert!(is_fleet_rx_port(*p));
        assert!(!RESERVED_PLANE_PORTS.contains(p));
    }
}

#[test]
fn config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<WfbConfig>(CONFIG_POINTER);
}

/// The status base block's `video.wfb` seed reads the same schema defaults the
/// radio runs at, so a node with no `video.wfb` block reports its configured
/// channel and power rather than zeros.
#[test]
fn wfb_status_config_slice_conforms() {
    ados_config::schema::assert_slice_conforms::<ados_protocol::wfb_status::WfbStatusConfig>(
        CONFIG_POINTER,
    );
}
