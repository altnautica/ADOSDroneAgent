use ados_sensorhub_proto::*;
use proptest::prelude::*;

fn roundtrip(pkt: &Packet<'_>) {
    let mut scratch = [0u8; MAX_RAW];
    let mut wire = [0u8; MAX_FRAME];
    let n = encode_frame(pkt, &mut scratch, &mut wire).unwrap();
    assert_eq!(wire[n - 1], 0);
    assert!(!wire[..n - 1].contains(&0));
    let mut buf = [0u8; MAX_RAW];
    let got = decode_frame(&wire[..n - 1], &mut buf).unwrap();
    assert_eq!(&got, pkt);
}

#[test]
fn crc_check_value() {
    assert_eq!(crc::crc16(b"123456789"), 0x29B1);
}

#[test]
fn payload_sizes_match_spec() {
    let mut raw = [0u8; MAX_RAW];
    let cases: [(Packet<'_>, usize); 8] = [
        (Packet::Hello(hello()), 40),
        (
            Packet::Baro(Baro {
                t_ns: 1,
                pressure_pa: 1.0,
                temp_c: 1.0,
                baro_id: 0,
            }),
            20,
        ),
        (
            Packet::Mag(Mag {
                t_ns: 1,
                field_ut: [1.0; 3],
                mag_id: 1,
            }),
            24,
        ),
        (
            Packet::Trigger(Trigger {
                seq: 1,
                cam_mask: 3,
                flags: 1,
                t_start_ns: 1,
                t_mid_ns: 2,
                exposure_ns: 3,
            }),
            28,
        ),
        (
            Packet::Pps(Pps {
                t_ns: 1,
                locked: 2,
                pps_count: 3,
                phase_err_ns: -4,
                freq_corr_ppb: 5,
            }),
            24,
        ),
        (Packet::Config(Config::default()), 16),
        (Packet::Health(health()), 40),
        (
            Packet::ImuBatch(ImuBatch {
                t0_ns: 0,
                dt_ns: 1,
                imu_id: 0,
                flags: 0,
                seq0: 0,
                samples: ImuSamples::Typed(&[ImuSample::default(); 64]),
            }),
            916,
        ),
    ];
    for (pkt, len) in cases {
        assert_eq!(
            encode_raw(&pkt, &mut raw).unwrap(),
            len + RAW_OVERHEAD,
            "{pkt:?}"
        );
        assert_eq!(u16::from_le_bytes([raw[1], raw[2]]) as usize, len);
    }
}

#[test]
fn trigger_byte_layout() {
    let pkt = Packet::Trigger(Trigger {
        seq: 0x0403_0201,
        cam_mask: 0x05,
        flags: 0x06,
        t_start_ns: 0x0f0e_0d0c_0b0a_0908,
        t_mid_ns: 0x1716_1514_1312_1110,
        exposure_ns: 0x1b1a_1918,
    });
    let mut raw = [0u8; 64];
    let n = encode_raw(&pkt, &mut raw).unwrap();
    assert_eq!(&raw[..3], &[0x20, 28, 0]);
    let p = &raw[3..31];
    assert_eq!(&p[..8], &[1, 2, 3, 4, 5, 6, 0, 0]);
    assert_eq!(&p[8..16], &[8, 9, 10, 11, 12, 13, 14, 15]);
    assert_eq!(&p[24..28], &[0x18, 0x19, 0x1a, 0x1b]);
    let crc = crc::crc16(&raw[..31]);
    assert_eq!(&raw[31..n], &crc.to_le_bytes());
}

#[test]
fn rejects_bad_frames() {
    let mut raw = [0u8; MAX_RAW];
    let n = encode_raw(&Packet::Config(Config::default()), &mut raw).unwrap();
    let mut bad = raw;
    bad[5] ^= 1;
    assert_eq!(decode_raw(&bad[..n]), Err(Error::Crc));
    assert_eq!(decode_raw(&raw[..n - 1]), Err(Error::Length));
    let mut big = [0u8; 8];
    big[1..3].copy_from_slice(&1025u16.to_le_bytes());
    assert_eq!(decode_raw(&big), Err(Error::Length));
    let unknown = Packet::Unknown {
        ty: 0x7f,
        payload: &[1, 2, 3],
    };
    roundtrip(&unknown);
    let gnss = [0u8; GNSS_MAX_BYTES + 1];
    let pkt = Packet::GnssRaw(GnssRaw {
        t_ns: 0,
        port: 0,
        bytes: &gnss,
    });
    assert_eq!(encode_raw(&pkt, &mut raw), Err(Error::Length));
}

#[test]
fn deframer_resyncs() {
    let mut scratch = [0u8; MAX_RAW];
    let mut wire = [0u8; MAX_FRAME];
    let pkt = Packet::Pps(Pps {
        t_ns: 9,
        locked: 2,
        pps_count: 1,
        phase_err_ns: 0,
        freq_corr_ppb: 0,
    });
    let n = encode_frame(&pkt, &mut scratch, &mut wire).unwrap();
    let mut d = Deframer::<64>::new();
    let mut stream = vec![0x11u8; 100];
    stream.push(0);
    stream.extend_from_slice(&wire[..n]);
    let mut got = 0;
    let mut overflow = 0;
    for &b in &stream {
        match d.push(b) {
            Push::Block(block) => {
                let mut buf = [0u8; MAX_RAW];
                assert_eq!(decode_frame(block, &mut buf).unwrap(), pkt);
                got += 1;
            }
            Push::Overflow => overflow += 1,
            Push::Pending => {}
        }
    }
    assert_eq!((got, overflow), (1, 1));
}

fn hello() -> Hello {
    Hello {
        proto_ver: PROTO_VER,
        fw_major: 1,
        fw_minor: 2,
        fw_patch: 3,
        board_id: BOARD_ID_SIM,
        caps: caps::IMU0 | caps::TRIGGER_OUT,
        uid: [1; 12],
        board_name: *b"sim\0\0\0\0\0\0\0\0\0\0\0\0\0",
    }
}

fn health() -> Health {
    Health {
        t_ns: 1,
        uptime_s: 2,
        mcu_temp_c: -3,
        imu_temp_c: 4500,
        vin_mv: 5000,
        usb_tx_drops: 6,
        imu_fifo_overflows: 7,
        rx_crc_errors: 8,
        trigger_missed: 9,
        pps_state: pps_state::HOLDOVER,
        heater_duty_pct: 50,
    }
}

fn sample() -> impl Strategy<Value = ImuSample> {
    (any::<[i16; 3]>(), any::<[i16; 3]>(), any::<i16>()).prop_map(|(gyro, accel, temp)| ImuSample {
        gyro,
        accel,
        temp,
    })
}

proptest! {
    #[test]
    fn cobs_roundtrip(data in proptest::collection::vec(any::<u8>(), 0..1100)) {
        let mut enc = vec![0u8; cobs::max_encoded_len(data.len())];
        let n = cobs::encode(&data, &mut enc).unwrap();
        prop_assert!(!enc[..n].contains(&0));
        let mut dec = vec![0u8; data.len()];
        let m = cobs::decode(&enc[..n], &mut dec).unwrap();
        prop_assert_eq!(&dec[..m], &data[..]);
    }

    #[test]
    fn decode_never_panics(data in proptest::collection::vec(any::<u8>(), 0..1200)) {
        let mut buf = [0u8; MAX_RAW];
        let _ = decode_frame(&data, &mut buf);
        let _ = decode_raw(&data);
    }

    #[test]
    fn single_bit_flip_is_rejected(seq in any::<u32>(), bit in 0usize..(33 * 8)) {
        let pkt = Packet::Trigger(Trigger { seq, cam_mask: 1, flags: 0, t_start_ns: 5, t_mid_ns: 6, exposure_ns: 7 });
        let mut raw = [0u8; 64];
        let n = encode_raw(&pkt, &mut raw).unwrap();
        raw[bit / 8] ^= 1 << (bit % 8);
        prop_assert!(decode_raw(&raw[..n]).is_err());
    }

    #[test]
    fn imu_roundtrip(t0 in any::<u64>(), dt in any::<u32>(), id in any::<u8>(), flags in any::<u8>(), seq0 in any::<u32>(),
                     samples in proptest::collection::vec(sample(), 1..=64)) {
        roundtrip(&Packet::ImuBatch(ImuBatch { t0_ns: t0, dt_ns: dt, imu_id: id, flags, seq0, samples: ImuSamples::Typed(&samples) }));
    }

    #[test]
    fn scalar_roundtrip(t in any::<u64>(), a in any::<u32>(), b in any::<i32>(), c in any::<u8>(), f in any::<[f32; 3]>()) {
        prop_assume!(f.iter().all(|v| !v.is_nan()));
        roundtrip(&Packet::Baro(Baro { t_ns: t, pressure_pa: f[0], temp_c: f[1], baro_id: c }));
        roundtrip(&Packet::Mag(Mag { t_ns: t, field_ut: f, mag_id: c }));
        roundtrip(&Packet::Pps(Pps { t_ns: t, locked: c, pps_count: a, phase_err_ns: b, freq_corr_ppb: -b }));
        roundtrip(&Packet::Trigger(Trigger { seq: a, cam_mask: c, flags: c, t_start_ns: t, t_mid_ns: t / 2, exposure_ns: a }));
        roundtrip(&Packet::Config(Config { trigger_hz: c.into(), exposure_us: a, imu_odr_hz: 1000, cam_mask: c, trigger_mode: 2, pps_out: 1, gprmc_out: 0, imu_batch: 10, heater_target_c: HEATER_OFF }));
        roundtrip(&Packet::Health(Health { t_ns: t, uptime_s: a, ..health() }));
        roundtrip(&Packet::Hello(Hello { caps: a, ..hello() }));
    }

    #[test]
    fn bytes_roundtrip(t in any::<u64>(), port in any::<u8>(), bytes in proptest::collection::vec(any::<u8>(), 0..=1014)) {
        roundtrip(&Packet::GnssRaw(GnssRaw { t_ns: t, port, bytes: &bytes }));
        roundtrip(&Packet::RtcmIn(RtcmIn { port, bytes: &bytes }));
    }
}
