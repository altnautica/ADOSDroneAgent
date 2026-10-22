use super::*;
use ados_sensorbus::{SyncObs, SyncSource};
use ados_sensorhub::{GnssRaw, ImuBatch, ImuSample, ImuSamples, Trigger};

fn run(conv: &mut Converter, st: &mut HubState, pkt: Packet<'_>, arrival: u64) -> Vec<BusFrame> {
    let mut out = Vec::new();
    conv.handle(&pkt, arrival, st, &mut |f| out.push(f));
    out
}

fn batch(samples: &[ImuSample], t0_ns: u64, seq0: u32) -> Packet<'_> {
    Packet::ImuBatch(ImuBatch {
        t0_ns,
        dt_ns: 1_000_000,
        imu_id: 0,
        flags: 0,
        seq0,
        samples: ImuSamples::Typed(samples),
    })
}

#[test]
fn imu_samples_are_scaled_rotated_and_stamped_behind_the_arrival() {
    let ext = Extrinsics::from_rig(&crate::rig::tests::rig_with_imu0()).unwrap();
    let mut conv = Converter::new(ext, true, false);
    let mut st = HubState::default();
    let s = ImuSample {
        gyro: [1000, 0, 0],
        accel: [0, 0, 2048],
        temp: 3150,
    };
    let samples = [s; 10];
    let frames = run(
        &mut conv,
        &mut st,
        batch(&samples, 5_000_000_000, 100),
        9_000_000_000,
    );
    assert_eq!(frames.len(), 1);
    assert_eq!(
        (frames[0].kind, frames[0].src.as_str()),
        (Kind::Imu, "imu0")
    );
    let b: v1::ImuBatch = frames[0].decode_proto().unwrap();
    assert_eq!(b.samples.len(), 10);
    // Before the fit: sample i sits (9 - i) ms before the arrival.
    assert_eq!(b.samples[0].t_ns, 9_000_000_000 - 9_000_000);
    assert_eq!(b.samples[9].t_ns, 9_000_000_000);
    assert_eq!(b.samples[3].seq, 103);
    // Chip x → body y under the rig's 90° yaw; 1000 LSB = 1000 · 2000/32768 °/s.
    let want = 1000.0 * proto::GYRO_LSB_RAD_S;
    let g = &b.samples[0].gyro;
    assert!(g[0].abs() < 1e-12 && (g[1] - want).abs() < 1e-12 && g[2].abs() < 1e-12);
    // 2048 LSB on z = 1 g.
    assert!((b.samples[0].accel[2] - 9.80665).abs() < 1e-9);
    assert_eq!(b.samples[0].temp_c, Some(31.5));
    assert_eq!(st.imu_samples, 10);

    // A skipped batch of 10 counts one dropped packet.
    run(
        &mut conv,
        &mut st,
        batch(&samples, 5_020_000_000, 120),
        9_020_000_000,
    );
    assert_eq!(st.dropped, 1);
}

#[test]
fn triggers_fan_out_per_camera_bit_and_gaps_count_as_dropped() {
    let mut conv = Converter::new(Extrinsics::default(), true, false);
    let mut st = HubState::default();
    let trig = |seq| {
        Packet::Trigger(Trigger {
            seq,
            cam_mask: 0b101,
            flags: 0,
            t_start_ns: 1_000_000,
            t_mid_ns: 3_000_000,
            exposure_ns: 4_000_000,
        })
    };
    let frames = run(&mut conv, &mut st, trig(7), 50_000_000);
    let srcs: Vec<&str> = frames.iter().map(|f| f.src.as_str()).collect();
    assert_eq!(srcs, ["cam0", "cam2"]);
    let rec: v1::TriggerRecord = frames[1].decode_proto().unwrap();
    assert_eq!(
        (rec.seq, rec.t_mid_ns, rec.exposure_ns, rec.gain),
        (7, 50_000_000, 4_000_000, 0.0)
    );
    run(&mut conv, &mut st, trig(10), 60_000_000);
    assert_eq!(st.dropped, 2);
    assert_eq!(st.triggers, 2);
}

#[test]
fn gnss_bytes_forward_only_when_enabled_and_ports_map_to_ids() {
    let gnss = |port, bytes: &'static [u8]| {
        Packet::GnssRaw(GnssRaw {
            t_ns: 1_000,
            port,
            bytes,
        })
    };
    let mut st = HubState::default();
    let mut off = Converter::new(Extrinsics::default(), false, false);
    assert!(run(&mut off, &mut st, gnss(3, b"$GP"), 10).is_empty());
    assert_eq!(off.rtcm_port(), 3);

    let mut on = Converter::new(Extrinsics::default(), true, false);
    let f = run(&mut on, &mut st, gnss(3, b"\xb5\x62"), 10);
    assert_eq!(f[0].src, "gnss0");
    let raw: v1::GnssRaw = f[0].decode_proto().unwrap();
    assert_eq!((raw.t_ns, raw.bytes.as_slice()), (10, &b"\xb5\x62"[..]));
    assert_eq!(run(&mut on, &mut st, gnss(1, b"x"), 11)[0].src, "gnss1");
}

#[test]
fn closed_buckets_publish_sync_pairs_and_hello_is_recorded() {
    let mut conv = Converter::new(Extrinsics::default(), true, false);
    let mut st = HubState::default();
    let health = |t_ns| {
        Packet::Health(proto::Health {
            t_ns,
            uptime_s: 12,
            mcu_temp_c: 0,
            imu_temp_c: 0,
            vin_mv: 5000,
            usb_tx_drops: 0,
            imu_fifo_overflows: 0,
            rx_crc_errors: 0,
            trigger_missed: 0,
            pps_state: proto::pps_state::LOCKED,
            heater_duty_pct: 0,
        })
    };
    assert!(run(&mut conv, &mut st, health(500), 1_000_000_500).is_empty());
    let f = run(&mut conv, &mut st, health(1_000_000_500), 2_000_001_000);
    assert_eq!(
        (f.len(), f[0].kind, f[0].src.as_str()),
        (1, Kind::Sync, SYNC_SRC)
    );
    let obs: SyncObs = f[0].decode_msgpack().unwrap();
    assert_eq!(
        obs,
        SyncObs {
            source: SyncSource::Hub,
            host_ns: 1_000_000_500,
            source_ns: 500,
            rtt_ns: 0,
            sim_time: false,
        }
    );
    assert_eq!(
        (st.uptime_s, st.pps_state),
        (Some(12), Some(proto::pps_state::LOCKED))
    );

    let mut name = [0u8; 16];
    name[..3].copy_from_slice(b"sim");
    let hello = |proto_ver| {
        Packet::Hello(proto::Hello {
            proto_ver,
            fw_major: 0,
            fw_minor: 1,
            fw_patch: 0,
            board_id: proto::BOARD_ID_SIM,
            caps: proto::caps::IMU0,
            uid: [0; 12],
            board_name: name,
        })
    };
    run(&mut conv, &mut st, hello(1), 2_000_002_000);
    let h = st.hello.clone().unwrap();
    assert_eq!(
        (h.fw.as_str(), h.board.as_str(), h.proto_ver),
        ("ados-sensorhub 0.1.0", "sim", 1)
    );
    assert_eq!(conv.unsupported_proto, None);
    run(&mut conv, &mut st, hello(2), 2_000_003_000);
    assert_eq!(conv.unsupported_proto, Some(2));
    // The state reports the version the hub announced.
    assert_eq!(st.hello.as_ref().unwrap().proto_ver, 2);
    // Packets after it are not read as v1: nothing published or counted.
    let samples = [ImuSample::default(); 4];
    let imu_before = st.imu_samples;
    assert!(run(
        &mut conv,
        &mut st,
        batch(&samples, 3_000_000_000, 0),
        4_000_000_000
    )
    .is_empty());
    assert!(run(&mut conv, &mut st, health(3_000_000_000), 5_000_000_000).is_empty());
    assert_eq!(st.imu_samples, imu_before);
}

#[test]
fn a_simulator_hub_marks_its_pairs_as_simulator_time() {
    let mut conv = Converter::new(Extrinsics::default(), true, true);
    let mut st = HubState::default();
    let hello = |board_id| {
        Packet::Hello(proto::Hello {
            proto_ver: proto::PROTO_VER,
            fw_major: 0,
            fw_minor: 1,
            fw_patch: 0,
            board_id,
            caps: proto::caps::IMU0,
            uid: [0; 12],
            board_name: [0; 16],
        })
    };
    let samples = [ImuSample::default(); 4];
    let sync_of = |frames: Vec<BusFrame>| -> SyncObs {
        let f = frames.iter().find(|f| f.kind == Kind::Sync).unwrap();
        f.decode_msgpack().unwrap()
    };
    run(&mut conv, &mut st, hello(proto::BOARD_ID_SIM), 500_000_000);
    run(
        &mut conv,
        &mut st,
        batch(&samples, 1_000_000_000, 0),
        1_000_000_500,
    );
    let f = run(
        &mut conv,
        &mut st,
        batch(&samples, 2_000_000_000, 4),
        2_000_000_500,
    );
    assert!(sync_of(f).sim_time);
    // A flight hub on the same link afterwards: its pairs are its own clock.
    run(
        &mut conv,
        &mut st,
        hello(proto::BOARD_ID_MATEK_H743_SLIM_V3),
        2_100_000_000,
    );
    let f = run(
        &mut conv,
        &mut st,
        batch(&samples, 3_000_000_000, 8),
        3_000_000_500,
    );
    assert!(!sync_of(f).sim_time);
}

#[test]
fn the_simulator_board_id_needs_simulation_enabled() {
    // 0xFFFF is also erased flash: off a simulator it is a hub's own clock.
    let mut conv = Converter::new(Extrinsics::default(), true, false);
    let mut st = HubState::default();
    let hello = Packet::Hello(proto::Hello {
        proto_ver: proto::PROTO_VER,
        fw_major: 0,
        fw_minor: 1,
        fw_patch: 0,
        board_id: proto::BOARD_ID_SIM,
        caps: proto::caps::IMU0,
        uid: [0; 12],
        board_name: [0; 16],
    });
    let samples = [ImuSample::default(); 4];
    run(&mut conv, &mut st, hello, 500_000_000);
    run(
        &mut conv,
        &mut st,
        batch(&samples, 1_000_000_000, 0),
        1_000_000_500,
    );
    let f = run(
        &mut conv,
        &mut st,
        batch(&samples, 2_000_000_000, 4),
        2_000_000_500,
    );
    let sync = f.iter().find(|f| f.kind == Kind::Sync).unwrap();
    let obs: SyncObs = sync.decode_msgpack().unwrap();
    assert!(!obs.sim_time);
}
