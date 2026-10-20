use super::*;
use ados_swarm_control::geo::Ned;

#[test]
fn a_setpoint_is_addressed_to_the_latched_autopilot_not_system_one() {
    // A fleet drone on slot 3 flies with system id 3; a setpoint for system
    // 1 is dropped by its autopilot while the counters say it was sent.
    use ados_protocol::mavlink::Message as _;
    let sp = Setpoint::velocity(Ned::new(1.0, 0.0, 0.0));
    let msg = setpoint_message((3, 1), &sp).expect("builds");
    assert_eq!(msg.target_system_id(), Some(3));
    assert_eq!(msg.target_component_id(), Some(1));
}

#[test]
fn no_autopilot_identity_is_known_before_its_heartbeat() {
    let state = Arc::new(Mutex::new(VehicleState::default()));
    let params = Arc::new(Mutex::new(crate::param_cache::ParamCache::new(
        "/tmp/ados-swarm-setpoint-test-params.json",
    )));
    let fc = FcConnection::new(crate::config::MavlinkConfig::default(), state, params);
    assert_eq!(fc.learned_fc_identity(), None);
    fc.latch_fc_identity(3, 1);
    assert_eq!(fc.learned_fc_identity(), Some((3, 1)));
}

#[test]
fn the_status_block_round_trips_every_precedence_level() {
    let s = SwarmSetpointStatus::default();
    // The default must be the honest pre-Phase-5 value, since the snapshot is
    // published from tick zero.
    assert_eq!(s.precedence_wire(), "hold");
    assert!(!s.emergency());
    for level in ModePrecedence::ARBITRATION_ORDER {
        s.publish(
            level,
            level == ModePrecedence::HardSeparation,
            Default::default(),
        );
        assert_eq!(s.precedence_wire(), level.as_wire(), "{level:?}");
    }
    assert_eq!(
        ModePrecedence::from_wire(s.precedence_wire()),
        ModePrecedence::Hold
    );
}

#[test]
fn the_status_block_publishes_the_controller_counters() {
    let s = SwarmSetpointStatus::default();
    s.publish(
        ModePrecedence::Flocking,
        true,
        ados_swarm_control::SwarmControlCounters {
            setpoints_emitted: 7,
            ticks_suppressed: 3,
            hard_engagements: 2,
        },
    );
    assert_eq!(s.setpoints_emitted(), 7);
    assert_eq!(s.ticks_suppressed(), 3);
    assert_eq!(s.hard_engagements(), 2);
    assert!(s.emergency());
}

#[test]
fn a_velocity_setpoint_builds_a_valid_global_int_message() {
    let sp = Setpoint::velocity(Ned::new(1.5, -2.5, -1.0));
    let wire = GuidedSetpoint {
        kind: WireSetpointKind::GlobalInt,
        coordinate_frame: sp.coordinate_frame(),
        type_mask: sp.type_mask(),
        x: sp.lat_e7 as f64,
        y: sp.lon_e7 as f64,
        z: sp.alt_m as f64,
        vx: sp.vn,
        vy: sp.ve,
        vz: sp.vd,
        afx: 0.0,
        afy: 0.0,
        afz: 0.0,
        yaw: 0.0,
        yaw_rate: 0.0,
    };
    wire.validate()
        .expect("the control layer must not emit an invalid setpoint");
    // The plan's message: SET_POSITION_TARGET_GLOBAL_INT, id 86.
    let msg = wire.build_message(1, 1).expect("builds");
    let bytes = ados_protocol::mavlink::serialize_v2(
        ados_protocol::mavlink::MavHeader {
            system_id: 1,
            component_id: 1,
            sequence: 0,
        },
        &msg,
    )
    .expect("serializes");
    assert_eq!(crate::aux_tee::mavlink_message_id(&bytes), Some(86));
}

#[test]
fn a_position_setpoint_also_builds_and_validates() {
    let sp = Setpoint::position(12.9716, 77.5946, 30.0);
    let wire = GuidedSetpoint {
        kind: WireSetpointKind::GlobalInt,
        coordinate_frame: sp.coordinate_frame(),
        type_mask: sp.type_mask(),
        x: sp.lat_e7 as f64,
        y: sp.lon_e7 as f64,
        z: sp.alt_m as f64,
        vx: sp.vn,
        vy: sp.ve,
        vz: sp.vd,
        afx: 0.0,
        afy: 0.0,
        afz: 0.0,
        yaw: 0.0,
        yaw_rate: 0.0,
    };
    wire.validate().expect("valid");
    assert!(wire.build_message(1, 1).is_ok());
}

/// Replace the file the way every config writer does: a temp file renamed
/// over the original.
fn replace(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

const HOME: (f64, f64, f64) = (12.9716, 77.5946, 30.0);

fn own_fresh() -> OwnState {
    OwnState {
        slot: 0,
        lat_deg: HOME.0,
        lon_deg: HOME.1,
        alt_rel_m: HOME.2,
        vn: 0.0,
        ve: 0.0,
        vd: 0.0,
        armed: true,
        guided: true,
        fix_age: Some(Duration::ZERO),
    }
}

/// Slot 2, `north_m` metres north of `own_fresh`, at the same altitude.
fn neighbour_north(north_m: f64) -> NeighborFix {
    use ados_swarm_control::neighbor::{STATUS_ARMED, STATUS_GPS_OK, STATUS_GUIDED};
    let (lat, lon, alt) = ados_swarm_control::GeoOrigin::new(HOME.0, HOME.1, HOME.2)
        .to_geo(Ned::new(north_m, 0.0, 0.0));
    NeighborFix {
        slot: 2,
        lat_deg: lat,
        lon_deg: lon,
        alt_m: alt,
        vn: 0.0,
        ve: 0.0,
        vd: 0.0,
        status: STATUS_ARMED | STATUS_GUIDED | STATUS_GPS_OK,
        sender_order: std::cmp::Ordering::Equal,
    }
}

const ENABLED_SLOT_1: &str = "swarm:\n  enabled: true\nvideo:\n  wfb:\n    fleet_slot: 1\n";

#[test]
fn a_raised_separation_envelope_reaches_the_running_controller() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    replace(&path, ENABLED_SLOT_1);
    let mut source = SwarmConfigSource::new(path.clone());
    let mut live = LiveSwarm::new(source.poll().expect("first poll reads"));
    let near = [neighbour_north(6.0)];

    // 6 m is outside the default 4 m hard floor.
    let before = live.tick(own_fresh(), &near, Instant::now());
    assert!(!before.emergency, "{before:?}");

    // The operator raises the envelope past the neighbour's distance.
    replace(
        &path,
        "swarm:\n  enabled: true\n  separation:\n    radius_m: 20\n    hard_m: 10\n\
             video:\n  wfb:\n    fleet_slot: 1\n",
    );
    live.reconfigure(source.poll().expect("a rewritten file is re-read"));

    let after = live.tick(own_fresh(), &near, Instant::now());
    assert!(after.emergency, "{after:?}");
    assert_eq!(after.precedence, ModePrecedence::HardSeparation);
}

#[test]
fn turning_participation_on_is_picked_up_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    replace(
        &path,
        "swarm:\n  enabled: false\nvideo:\n  wfb:\n    fleet_slot: 1\n",
    );
    let mut source = SwarmConfigSource::new(path.clone());
    assert!(!eligible(&source.poll().expect("first poll reads")));

    replace(&path, ENABLED_SLOT_1);
    let next = source.poll().expect("a rewritten file is re-read");
    assert!(eligible(&next));

    // And off again: the running loop stands down on the next poll.
    replace(
        &path,
        "swarm:\n  enabled: false\nvideo:\n  wfb:\n    fleet_slot: 1\n",
    );
    assert!(!eligible(
        &source.poll().expect("a rewritten file is re-read")
    ));
}

#[test]
fn an_unchanged_file_is_not_reparsed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    replace(&path, ENABLED_SLOT_1);
    let mut source = SwarmConfigSource::new(path.clone());
    assert!(source.poll().is_some());
    assert!(source.poll().is_none());
    // An absent file reads as the default block once, then stays quiet.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(source.poll(), Some(SwarmControlConfig::default()));
    assert!(source.poll().is_none());
}

#[test]
fn a_ground_station_slot_disqualifies_the_layer() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // Enabled, but no drone slot: slot 0 is the ground station.
    std::fs::write(&cfg, "swarm:\n  enabled: true\n").unwrap();
    assert!(!eligible(&SwarmControlConfig::load_from(&cfg)));
    // With a slot assigned it is eligible.
    std::fs::write(
        &cfg,
        "swarm:\n  enabled: true\nvideo:\n  wfb:\n    fleet_slot: 4\n",
    )
    .unwrap();
    assert!(eligible(&SwarmControlConfig::load_from(&cfg)));
}

#[test]
fn standing_down_clears_the_advertised_level() {
    let s = SwarmSetpointStatus::default();
    s.publish(ModePrecedence::HardSeparation, true, Default::default());
    s.stand_down();
    assert_eq!(s.precedence_wire(), "hold");
    assert!(!s.emergency());
}

/// A fresh sidecar claimed by an operator holds the layer off, and the
/// beacon says the operator is flying.
#[test]
fn a_fresh_operator_claim_stands_the_layer_down() {
    use super::super::link_state::pic_refuses_swarm;
    use ados_hid::pic_view::PicView;
    let held = PicView {
        claimed: true,
        holder: Some("op-a".into()),
    };
    assert!(pic_refuses_swarm(Some(&held)));
    let s = SwarmSetpointStatus::default();
    s.publish(ModePrecedence::Formation, false, Default::default());
    s.stand_down_for_operator();
    assert_eq!(s.precedence_wire(), "operator");
    // Unclaimed, or no sidecar at all, keeps today's behaviour.
    assert!(!pic_refuses_swarm(Some(&PicView::default())));
    assert!(!pic_refuses_swarm(None));
}

#[tokio::test]
async fn the_unsigned_command_risk_holds_the_layer_off_once() {
    let fc = FcConnection::new(
        crate::config::MavlinkConfig::default(),
        Arc::new(Mutex::new(VehicleState::default())),
        Arc::new(Mutex::new(crate::param_cache::ParamCache::new(
            std::env::temp_dir().join("ados-swarm-signing-test-params.json"),
        ))),
    );
    let s = SwarmSetpointStatus::default();
    // No open link: unmeasured, so no hold.
    assert!(!signing_holds(&fc, &s).await);
    assert!(!s.signing_hold_logged.load(Ordering::Relaxed));
    // A signing FC on a UART: the layer is not eligible.
    fc.connected.store(true, Ordering::Relaxed);
    *fc.fc_device.lock().await = Some("/dev/ttyAMA0".into());
    fc.note_vehicle_frame(&[0xFD, 9, 0x01, 0]);
    assert!(signing_holds(&fc, &s).await);
    assert!(s.signing_hold_logged.load(Ordering::Relaxed));
}

#[tokio::test]
async fn a_visor_sends_no_setpoint_and_counts_no_client_refusal() {
    use tokio::io::AsyncReadExt;
    for (profile, sends) in [("visor", false), ("drone", true)] {
        let cfg = crate::config::MavlinkConfig {
            profile: profile.into(),
            ..crate::config::MavlinkConfig::default()
        };
        let fc = FcConnection::new(
            cfg,
            Arc::new(Mutex::new(VehicleState::default())),
            Arc::new(Mutex::new(crate::param_cache::ParamCache::new(
                std::env::temp_dir().join("ados-swarm-visor-test-params.json"),
            ))),
        );
        let (fc_side, mut far) = tokio::io::duplex(4096);
        fc.connected.store(true, Ordering::Relaxed);
        *fc.writer.lock().await = Some(Box::pin(fc_side));
        let sp = Setpoint::velocity(Ned::new(1.0, 0.0, 0.0));
        send_setpoint(&fc, (1, 1), &sp).await;
        let mut buf = [0u8; 64];
        let wait = std::time::Duration::from_millis(50);
        let read = tokio::time::timeout(wait, far.read(&mut buf)).await;
        assert_eq!(read.is_ok(), sends, "{profile}");
        assert_eq!(fc.visor_flight_frames_dropped(), 0, "{profile}");
    }
}
