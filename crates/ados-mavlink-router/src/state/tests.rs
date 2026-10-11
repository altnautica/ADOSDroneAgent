use super::*;
use ados_protocol::mavlink::ardupilotmega::{
    GpsFixType, MavAutopilot, MavBatteryFunction, MavBatteryType, MavModeFlag, MavSeverity,
    MavState, MavSysStatusSensor, MavType, BATTERY_STATUS_DATA, GLOBAL_POSITION_INT_DATA,
    GPS_RAW_INT_DATA, HEARTBEAT_DATA, PARAM_VALUE_DATA, RC_CHANNELS_DATA, STATUSTEXT_DATA,
    SYS_STATUS_DATA, VFR_HUD_DATA,
};
use ados_protocol::mavlink::MavMessage;

const TS: &str = "2026-05-28T15:28:23.880948+00:00";

fn heartbeat(mavtype: MavType, custom_mode: u32, armed: bool) -> MavMessage {
    let base_mode = if armed {
        MavModeFlag::MAV_MODE_FLAG_SAFETY_ARMED
    } else {
        MavModeFlag::empty()
    };
    MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode,
        mavtype,
        autopilot: MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA,
        base_mode,
        system_status: MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    })
}

/// The banner STATUSTEXT ArduPilot sends at boot and on a param request.
fn banner(text: &str) -> MavMessage {
    let mut buf = [0u8; 50];
    buf[..text.len()].copy_from_slice(text.as_bytes());
    MavMessage::STATUSTEXT(STATUSTEXT_DATA {
        severity: MavSeverity::MAV_SEVERITY_INFO,
        text: buf.into(),
    })
}

#[test]
fn heartbeat_sets_mode_armed_and_type() {
    let mut s = VehicleState::default();
    let r = s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 4, true), TS);
    assert!(r.is_none());
    assert_eq!(s.mode, "GUIDED");
    assert!(s.armed);
    assert_eq!(s.last_heartbeat, TS);
    assert_eq!(s.last_update, TS);
    assert_eq!(
        s.autopilot,
        MavAutopilot::MAV_AUTOPILOT_ARDUPILOTMEGA as i64
    );
}

#[test]
fn unknown_mode_falls_back_to_mode_number() {
    let mut s = VehicleState::default();
    // custom_mode 99 is not in the copter table.
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 99, false), TS);
    assert_eq!(s.mode, "MODE_99");
    assert!(!s.armed);
}

#[test]
fn a_quadplane_is_decoded_with_the_plane_table_once_its_banner_arrives() {
    // Q_MAV_TYPE=QUADROTOR makes a plane heartbeat as a quad. Mode 6 is
    // FBWB on Plane and RTL on Copter: before the banner only the type's
    // suggestion is displayed and no firmware is published, so the command
    // route refuses to encode a mode; after it the plane name is reported
    // and the snapshot carries the firmware.
    let mut s = VehicleState::default();
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 6, false), TS);
    assert_eq!(s.to_wire()["vehicle_firmware"], Value::Null);
    s.update_from_message(&banner("ArduPlane V4.5.7 (4a6ff3b2)"), TS);
    assert_eq!(s.mode, "FBWB");
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 11, false), TS);
    assert_eq!(s.mode, "RTL");
    assert_eq!(s.to_wire()["vehicle_firmware"], json!("plane"));
    // Another multirotor type contradicts nothing; a type only another
    // firmware reports is a different FC and drops the identification.
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_HEXAROTOR, 6, false), TS);
    assert_eq!(s.mode, "FBWB");
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_GROUND_ROVER, 6, false), TS);
    assert_eq!(s.firmware, None);
    assert_eq!(s.mode, "FOLLOW");
}

fn heartbeat_px4(mavtype: MavType, custom_mode: u32) -> MavMessage {
    MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode,
        mavtype,
        autopilot: MavAutopilot::MAV_AUTOPILOT_PX4,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_STANDBY,
        mavlink_version: 3,
    })
}

/// PX4 packs the mode differently (main bits 16-23, sub bits 24-31), so an
/// AUTO submode must decode via the PX4 path, not the ArduCopter table.
fn px4_custom(main: u32, sub: u32) -> u32 {
    (sub << 24) | (main << 16)
}

#[test]
fn px4_heartbeat_decodes_base_and_auto_submodes() {
    let mut s = VehicleState::default();
    // POSCTL (main=3, sub=0).
    s.update_from_message(
        &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(3, 0)),
        TS,
    );
    assert_eq!(s.mode, "POSCTL");
    // AUTO.MISSION (main=4, sub=4) -> 0x04040000, wrong on the copter table.
    s.update_from_message(
        &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(4, 4)),
        TS,
    );
    assert_eq!(s.mode, "AUTO.MISSION");
    // AUTO.RTL (main=4, sub=5).
    s.update_from_message(
        &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(4, 5)),
        TS,
    );
    assert_eq!(s.mode, "AUTO.RTL");
    // A PX4 fixed-wing still uses the PX4 path (keyed on autopilot, not type).
    s.update_from_message(
        &heartbeat_px4(MavType::MAV_TYPE_FIXED_WING, px4_custom(4, 4)),
        TS,
    );
    assert_eq!(s.mode, "AUTO.MISSION");
}

#[test]
fn px4_unmapped_submode_falls_back_to_mode_number() {
    let mut s = VehicleState::default();
    // AUTO main (4) with an unmapped sub (99). There is no (4, 0) entry to
    // fall back to, so the mode degrades to MODE_<custom_mode>.
    s.update_from_message(
        &heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, px4_custom(4, 99)),
        TS,
    );
    assert_eq!(s.mode, format!("MODE_{}", px4_custom(4, 99)));
}

#[test]
fn plane_and_rover_tables_select_correctly() {
    let mut s = VehicleState::default();
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_FIXED_WING, 5, false), TS);
    assert_eq!(s.mode, "FBWA");
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_GROUND_ROVER, 4, false), TS);
    assert_eq!(s.mode, "HOLD");
}

#[test]
fn global_position_scaling_matches_python() {
    let mut s = VehicleState::default();
    s.update_from_message(
        &MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
            time_boot_ms: 0,
            lat: 129_716_000,
            lon: 775_946_000,
            alt: 120_000,
            relative_alt: 50_000,
            vx: 250,
            vy: -100,
            vz: 30,
            hdg: 18_000,
        }),
        TS,
    );
    assert!((s.lat - 12.9716).abs() < 1e-9);
    assert!((s.lon - 77.5946).abs() < 1e-9);
    assert!((s.alt_msl - 120.0).abs() < 1e-9);
    assert!((s.alt_rel - 50.0).abs() < 1e-9);
    assert!((s.vx - 2.5).abs() < 1e-9);
    assert!((s.heading - 180.0).abs() < 1e-9);
}

#[test]
fn a_heartbeat_alone_does_not_advance_the_position_stamp() {
    // The whole reason `position_at` exists rather than reusing
    // `last_update`. A vehicle whose position stream has stopped while its
    // heartbeat continues still looks fully alive by `last_update`, which is
    // refreshed by every frame before the match. Anything gating on
    // freshness has to see the position specifically or it will fly on a
    // frozen fix.
    let mut s = VehicleState::default();
    assert!(
        s.position_at.is_none(),
        "no position seen yet is not 'just now'"
    );

    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 0, true), TS);
    assert!(
        s.position_at.is_none(),
        "a heartbeat is not a position report"
    );
    assert!(
        !s.last_update.is_empty(),
        "the message stream IS fresh, which is exactly why it cannot be the signal"
    );

    s.update_from_message(
        &MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
            time_boot_ms: 0,
            lat: 129_716_000,
            lon: 775_946_000,
            alt: 120_000,
            relative_alt: 50_000,
            vx: 0,
            vy: 0,
            vz: 0,
            hdg: 0,
        }),
        TS,
    );
    let first = s.position_at.expect("a position report stamps it");

    // A later heartbeat must not move it forward.
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 0, true), TS);
    assert_eq!(
        s.position_at,
        Some(first),
        "a heartbeat after a position must not refresh the position stamp"
    );
}

#[test]
fn the_wire_snapshot_reports_the_position_age_an_out_of_process_consumer_gates_on() {
    // The in-process `Instant` is invisible to the swarm bus, which runs in
    // another process and has to decide whether this node's fix is alive
    // enough to radiate to the fleet. Before the age was published it could
    // only ask "did a state frame arrive recently", which a heartbeat
    // answers yes to on a vehicle whose GPS has died.
    let mut s = VehicleState::default();
    assert_eq!(
        s.to_wire()["position_age_ms"],
        json!(null),
        "no position ever decoded must not read as a fresh fix"
    );

    // A heartbeat refreshes `last_update` and nothing else, so the age is
    // still absent while the frame stream is demonstrably live.
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 0, true), TS);
    let wire = s.to_wire();
    assert!(!wire["last_update"].as_str().unwrap().is_empty());
    assert_eq!(wire["position_age_ms"], json!(null));

    s.update_from_message(
        &MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
            time_boot_ms: 0,
            lat: 129_716_000,
            lon: 775_946_000,
            alt: 120_000,
            relative_alt: 50_000,
            vx: 0,
            vy: 0,
            vz: 0,
            hdg: 0,
        }),
        TS,
    );
    let fresh = s.to_wire()["position_age_ms"]
        .as_u64()
        .expect("a decoded position publishes an age");
    assert!(fresh < 1_000, "a just-decoded fix is young, got {fresh}ms");

    // Age is derived at publish time, so a snapshot taken later reports a
    // larger age from the SAME position — that monotonic growth is the whole
    // signal a stale-fix gate reads.
    std::thread::sleep(std::time::Duration::from_millis(20));
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 0, true), TS);
    let later = s.to_wire()["position_age_ms"].as_u64().unwrap();
    assert!(
        later >= fresh + 15,
        "a heartbeat must not rejuvenate the fix age: {fresh}ms -> {later}ms"
    );
}

#[test]
fn battery_status_filters_unfilled_cells_and_temp_sentinel() {
    let mut s = VehicleState::default();
    let mut voltages = [0xFFFFu16; 10];
    voltages[0] = 4200;
    voltages[1] = 4180;
    s.update_from_message(
        &MavMessage::BATTERY_STATUS(BATTERY_STATUS_DATA {
            current_consumed: 1500,
            energy_consumed: 9000,
            temperature: 0x7FFF,
            voltages,
            current_battery: 0,
            id: 0,
            battery_function: MavBatteryFunction::MAV_BATTERY_FUNCTION_ALL,
            mavtype: MavBatteryType::MAV_BATTERY_TYPE_LIPO,
            battery_remaining: 0,
        }),
        TS,
    );
    assert_eq!(s.battery_voltages, vec![4.2, 4.18]);
    assert_eq!(s.battery_temperature, None);
    assert_eq!(s.battery_current_consumed, 1500);
}

/// Each BATTERY_STATUS id is its own pack on the wire, ordered by id, and a
/// later frame for one id replaces only that pack. A ninth distinct id is
/// ignored so the snapshot cannot grow without bound.
#[test]
fn battery_status_keeps_one_pack_per_id_in_id_order() {
    let status = |id: u8, cells: &[u16], current_ca: i16, remaining: i8| {
        let mut voltages = [u16::MAX; 10];
        voltages[..cells.len()].copy_from_slice(cells);
        MavMessage::BATTERY_STATUS(BATTERY_STATUS_DATA {
            current_consumed: 1200,
            energy_consumed: 3600,
            temperature: 3150,
            voltages,
            current_battery: current_ca,
            id,
            battery_function: MavBatteryFunction::MAV_BATTERY_FUNCTION_ALL,
            mavtype: MavBatteryType::MAV_BATTERY_TYPE_LIPO,
            battery_remaining: remaining,
        })
    };
    let mut s = VehicleState::default();
    s.update_from_message(&status(1, &[3900, 3910, 3905], 1250, 70), TS);
    s.update_from_message(&status(0, &[4100, 4090, 4080, 4070], 2000, 80), TS);
    s.update_from_message(&status(1, &[3880, 3890, 3885], 1300, 69), TS);

    let wire = s.to_wire();
    let packs = wire["batteries"].as_array().unwrap();
    assert_eq!(packs.len(), 2);
    assert_eq!(packs[0]["id"], json!(0));
    assert_eq!(packs[0]["cell_voltages"], json!([4.1, 4.09, 4.08, 4.07]));
    assert_eq!(packs[0]["current_a"], json!(20.0));
    assert_eq!(packs[0]["remaining_pct"], json!(80));
    assert_eq!(packs[1]["id"], json!(1));
    assert_eq!(packs[1]["cell_voltages"], json!([3.88, 3.89, 3.885]));
    assert_eq!(packs[1]["current_a"], json!(13.0));
    assert_eq!(packs[1]["remaining_pct"], json!(69));
    assert_eq!(packs[1]["temperature_c"], json!(31.5));
    assert_eq!(packs[1]["consumed_mah"], json!(1200));
    assert_eq!(packs[1]["consumed_wh"], json!(100.0));
    // The single-pack view follows the most recent frame of any id.
    assert_eq!(wire["battery"]["cell_voltages"], json!([3.88, 3.89, 3.885]));

    for id in 2..=9 {
        s.update_from_message(&status(id, &[3900], 0, 50), TS);
    }
    let ids: Vec<u8> = s.battery_packs.keys().copied().collect();
    assert_eq!(ids, (0..MAX_BATTERY_PACKS as u8).collect::<Vec<_>>());
}

#[test]
fn param_value_returns_persist_tuple_and_trims_nulls() {
    let mut s = VehicleState::default();
    let mut param_id = [0u8; 16];
    param_id[..5].copy_from_slice(b"WPNAV");
    let r = s.update_from_message(
        &MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
            param_value: 1234.5,
            param_count: 700,
            param_index: 1,
            param_id: param_id.into(),
            param_type: ados_protocol::mavlink::ardupilotmega::MavParamType::MAV_PARAM_TYPE_REAL32,
        }),
        TS,
    );
    let (name, value, _ptype) = r.expect("PARAM_VALUE yields a persist tuple");
    assert_eq!(name, "WPNAV");
    assert_eq!(value, 1234.5);
    assert_eq!(s.param_count, 700);
}

#[test]
fn a_px4_integer_param_is_decoded_from_its_bytes() {
    use ados_protocol::mavlink::ardupilotmega::MavParamType;
    let mut s = VehicleState::default();
    s.update_from_message(&heartbeat_px4(MavType::MAV_TYPE_QUADROTOR, 0), TS);
    let mut param_id = [0u8; 16];
    param_id[..8].copy_from_slice(b"COM_RC_L");
    let value_frame = |raw: f32, param_type: MavParamType| {
        MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
            param_value: raw,
            param_count: 1,
            param_index: 0,
            param_id: param_id.into(),
            param_type,
        })
    };
    let r = s.update_from_message(
        &value_frame(
            f32::from_le_bytes(5i32.to_le_bytes()),
            MavParamType::MAV_PARAM_TYPE_INT32,
        ),
        TS,
    );
    let (_name, value, ptype) = r.unwrap();
    assert_eq!(value, 5.0, "PX4 packs INT32 bytewise");
    assert_eq!(ptype, 6);
    // A float param on the same vehicle is the float itself.
    let (_n, value, _t) = s
        .update_from_message(&value_frame(0.25, MavParamType::MAV_PARAM_TYPE_REAL32), TS)
        .unwrap();
    assert_eq!(value, 0.25);
}

#[test]
fn an_ardupilot_integer_param_is_the_float_it_sent() {
    use ados_protocol::mavlink::ardupilotmega::MavParamType;
    let mut s = VehicleState::default();
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 0, false), TS);
    let r = s.update_from_message(
        &MavMessage::PARAM_VALUE(PARAM_VALUE_DATA {
            param_value: 5.0,
            param_count: 1,
            param_index: 0,
            param_id: [0u8; 16].into(),
            param_type: MavParamType::MAV_PARAM_TYPE_INT32,
        }),
        TS,
    );
    assert_eq!(r.unwrap().1, 5.0);
}

#[test]
fn to_wire_has_the_python_to_dict_shape() {
    let mut s = VehicleState::default();
    s.update_from_message(&heartbeat(MavType::MAV_TYPE_QUADROTOR, 0, false), TS);
    let wire = s.to_wire();
    // Top-level keys present, nested groups shaped like to_dict().
    for key in [
        "mav_type",
        "autopilot",
        "armed",
        "mode",
        "position",
        "velocity",
        "attitude",
        "battery",
        "batteries",
        "gps",
        "rc",
        "throttle",
        "last_heartbeat",
        "last_update",
        "position_age_ms",
    ] {
        assert!(wire.get(key).is_some(), "missing wire key {key}");
    }
    assert_eq!(wire["mode"], json!("STABILIZE"));
    assert!(wire["battery"].get("cell_voltages").is_some());
    assert!(
        wire["attitude"].get("rollspeed").is_none(),
        "to_dict omits rollspeed"
    );
    assert_eq!(wire["rc"]["channels"].as_array().unwrap().len(), 18);
}

#[test]
fn to_wire_with_merges_runtime_extras() {
    let s = VehicleState::default();
    let mut extras = Map::new();
    extras.insert("fc_connected".into(), json!(true));
    extras.insert("param_priming".into(), json!(false));
    extras.insert("service_uptime".into(), json!(12.5));
    let wire = s.to_wire_with(&extras);
    assert_eq!(wire["fc_connected"], json!(true));
    assert_eq!(wire["param_priming"], json!(false));
    assert_eq!(wire["service_uptime"], json!(12.5));
    // Base fields still present.
    assert!(wire.get("position").is_some());
}

#[test]
fn sys_status_and_gps_and_vfr_scaling() {
    let mut s = VehicleState::default();
    s.update_from_message(
        &MavMessage::SYS_STATUS(SYS_STATUS_DATA {
            onboard_control_sensors_present: MavSysStatusSensor::empty(),
            onboard_control_sensors_enabled: MavSysStatusSensor::empty(),
            onboard_control_sensors_health: MavSysStatusSensor::MAV_SYS_STATUS_SENSOR_3D_GYRO,
            load: 500,
            voltage_battery: 16400,
            current_battery: 250,
            drop_rate_comm: 0,
            errors_comm: 0,
            errors_count1: 0,
            errors_count2: 0,
            errors_count3: 0,
            errors_count4: 0,
            battery_remaining: 87,
        }),
        TS,
    );
    assert!((s.voltage_battery.unwrap() - 16.4).abs() < 1e-9);
    assert!((s.current_battery.unwrap() - 2.5).abs() < 1e-9);
    assert_eq!(s.battery_remaining, Some(87));

    s.update_from_message(
        &MavMessage::GPS_RAW_INT(GPS_RAW_INT_DATA {
            time_usec: 0,
            lat: 0,
            lon: 0,
            alt: 0,
            eph: 150,
            epv: 200,
            vel: 0,
            cog: 0,
            fix_type: GpsFixType::GPS_FIX_TYPE_3D_FIX,
            satellites_visible: 14,
        }),
        TS,
    );
    assert_eq!(s.gps_satellites, 14);
    assert!((s.gps_eph.unwrap() - 1.5).abs() < 1e-9);
    assert_eq!(s.gps_fix_type, GpsFixType::GPS_FIX_TYPE_3D_FIX as i64);

    s.update_from_message(
        &MavMessage::VFR_HUD(VFR_HUD_DATA {
            airspeed: 12.0,
            groundspeed: 11.0,
            alt: 100.0,
            climb: 1.5,
            heading: 90,
            throttle: 55,
        }),
        TS,
    );
    assert_eq!(s.throttle, 55);
    assert!((s.groundspeed - 11.0).abs() < 1e-9);
}

#[test]
fn rc_channels_capture_all_18_and_rssi() {
    let mut s = VehicleState::default();
    s.update_from_message(
        &MavMessage::RC_CHANNELS(RC_CHANNELS_DATA {
            time_boot_ms: 0,
            chan1_raw: 1500,
            chan2_raw: 1501,
            chan3_raw: 1000,
            chan4_raw: 1500,
            chan5_raw: 1,
            chan6_raw: 2,
            chan7_raw: 3,
            chan8_raw: 4,
            chan9_raw: 5,
            chan10_raw: 6,
            chan11_raw: 7,
            chan12_raw: 8,
            chan13_raw: 9,
            chan14_raw: 10,
            chan15_raw: 11,
            chan16_raw: 12,
            chan17_raw: 13,
            chan18_raw: 14,
            chancount: 18,
            rssi: 200,
        }),
        TS,
    );
    assert_eq!(s.rc_channels.len(), 18);
    assert_eq!(s.rc_channels[0], 1500);
    assert_eq!(s.rc_channels[17], 14);
    assert_eq!(s.rc_rssi, Some(200));
}

#[test]
fn firmware_family_maps_only_verified_signals() {
    // MSP families come from the USB descriptor variant, regardless of the
    // (absent) heartbeat.
    assert_eq!(firmware_family(Some("betaflight"), false, 0), "betaflight");
    assert_eq!(firmware_family(Some("inav"), false, 0), "inav");
    // A MAVLink FC is named by the autopilot code, but ONLY while alive.
    assert_eq!(firmware_family(None, true, 3), "ardupilot");
    assert_eq!(firmware_family(None, true, 12), "px4");
    // Not alive → the autopilot code is stale, so never claim a family.
    assert_eq!(firmware_family(None, false, 3), "unknown");
    assert_eq!(firmware_family(None, false, 12), "unknown");
    // Alive but an unrecognised autopilot code, and an unrecognised MSP
    // descriptor, both stay honest.
    assert_eq!(firmware_family(None, true, 0), "unknown");
    assert_eq!(firmware_family(Some("cleanflight"), false, 0), "unknown");
}

/// A ground station's HEARTBEAT as a flight controller forwards it: its own
/// system id, `MAV_TYPE_GCS`, no autopilot, base mode 0.
fn gcs_heartbeat() -> MavMessage {
    MavMessage::HEARTBEAT(HEARTBEAT_DATA {
        custom_mode: 0,
        mavtype: MavType::MAV_TYPE_GCS,
        autopilot: MavAutopilot::MAV_AUTOPILOT_INVALID,
        base_mode: MavModeFlag::empty(),
        system_status: MavState::MAV_STATE_ACTIVE,
        mavlink_version: 3,
    })
}

#[test]
fn only_an_autopilot_heartbeat_identifies_the_vehicle() {
    let mut src = VehicleSource::default();
    let position = MavMessage::GLOBAL_POSITION_INT(GLOBAL_POSITION_INT_DATA {
        time_boot_ms: 0,
        lat: 1,
        lon: 1,
        alt: 0,
        relative_alt: 0,
        vx: 0,
        vy: 0,
        vz: 0,
        hdg: 0,
    });
    // Nothing is the vehicle before its HEARTBEAT.
    assert!(!src.admit(1, 1, &position));
    // A forwarded ground station, and a companion sharing the vehicle's
    // system id, are not the vehicle.
    assert!(!src.admit(255, 190, &gcs_heartbeat()));
    assert!(!src.admit(1, 191, &gcs_heartbeat()));
    assert_eq!(src.system_id(), None);

    assert!(src.admit(1, 1, &heartbeat(MavType::MAV_TYPE_QUADROTOR, 4, true)));
    assert_eq!(src.system_id(), Some(1));
    assert!(src.admit(1, 1, &position));
    // Telemetry from another system, or another component of the same
    // system, never feeds the vehicle's state.
    assert!(!src.admit(255, 190, &position));
    assert!(!src.admit(1, 154, &position));
    // A later ground-station HEARTBEAT does not move the latch.
    assert!(!src.admit(255, 190, &gcs_heartbeat()));
    assert_eq!(src.system_id(), Some(1));
}

/// Every MAVLink "not measured" sentinel is published as `null`, never as a
/// reading: no current sensor is not -0.01 A, an unknown temperature is not
/// 0 C, an unknown accuracy is not 655 m.
#[test]
fn not_reported_sentinels_are_published_as_null() {
    let mut s = VehicleState::default();
    s.update_from_message(
        &MavMessage::SYS_STATUS(SYS_STATUS_DATA {
            onboard_control_sensors_present: MavSysStatusSensor::empty(),
            onboard_control_sensors_enabled: MavSysStatusSensor::empty(),
            onboard_control_sensors_health: MavSysStatusSensor::empty(),
            load: 0,
            voltage_battery: u16::MAX,
            current_battery: -1,
            drop_rate_comm: 0,
            errors_comm: 0,
            errors_count1: 0,
            errors_count2: 0,
            errors_count3: 0,
            errors_count4: 0,
            battery_remaining: -1,
        }),
        TS,
    );
    s.update_from_message(
        &MavMessage::GPS_RAW_INT(GPS_RAW_INT_DATA {
            time_usec: 0,
            lat: 0,
            lon: 0,
            alt: 0,
            eph: u16::MAX,
            epv: u16::MAX,
            vel: 0,
            cog: 0,
            fix_type: GpsFixType::GPS_FIX_TYPE_NO_FIX,
            satellites_visible: 0,
        }),
        TS,
    );
    s.update_from_message(
        &MavMessage::BATTERY_STATUS(BATTERY_STATUS_DATA {
            current_consumed: -1,
            energy_consumed: -1,
            temperature: i16::MAX,
            voltages: [u16::MAX; 10],
            current_battery: -1,
            id: 0,
            battery_function: MavBatteryFunction::MAV_BATTERY_FUNCTION_ALL,
            mavtype: MavBatteryType::MAV_BATTERY_TYPE_LIPO,
            battery_remaining: -1,
        }),
        TS,
    );
    s.update_from_message(
        &MavMessage::RC_CHANNELS(RC_CHANNELS_DATA {
            time_boot_ms: 0,
            chan1_raw: 1500,
            chan2_raw: 0,
            chan3_raw: 0,
            chan4_raw: 0,
            chan5_raw: 0,
            chan6_raw: 0,
            chan7_raw: 0,
            chan8_raw: 0,
            chan9_raw: 0,
            chan10_raw: 0,
            chan11_raw: 0,
            chan12_raw: 0,
            chan13_raw: 0,
            chan14_raw: 0,
            chan15_raw: 0,
            chan16_raw: 0,
            chan17_raw: 0,
            chan18_raw: 0,
            chancount: 1,
            rssi: u8::MAX,
        }),
        TS,
    );
    let wire = s.to_wire();
    for (group, key) in [
        ("battery", "voltage"),
        ("battery", "current"),
        ("battery", "remaining"),
        ("battery", "temperature"),
        ("gps", "eph"),
        ("gps", "epv"),
        ("rc", "rssi"),
    ] {
        assert_eq!(wire[group][key], Value::Null, "{group}.{key}");
    }
    let pack = &wire["batteries"][0];
    assert_eq!(pack["cell_voltages"], json!([]));
    for key in [
        "current_a",
        "remaining_pct",
        "temperature_c",
        "consumed_mah",
        "consumed_wh",
    ] {
        assert_eq!(pack[key], Value::Null, "batteries[0].{key}");
    }
}
