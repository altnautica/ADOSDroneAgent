use super::*;
use ados_sensorhub::*;
use proptest::prelude::*;

/// An owned packet, so proptest can generate it and decoded packets can
/// outlive the decoder's buffer.
#[derive(Debug, Clone, PartialEq)]
enum Owned {
    Hello(Hello),
    Imu {
        t0_ns: u64,
        dt_ns: u32,
        imu_id: u8,
        flags: u8,
        seq0: u32,
        samples: Vec<ImuSample>,
    },
    Baro(Baro),
    Mag(Mag),
    Trigger(Trigger),
    Pps(Pps),
    Gnss {
        t_ns: u64,
        port: u8,
        bytes: Vec<u8>,
    },
    Rtcm {
        port: u8,
        bytes: Vec<u8>,
    },
    Config(Config),
    Health(Health),
}

impl Owned {
    fn as_packet(&self) -> Packet<'_> {
        match self {
            Owned::Hello(h) => Packet::Hello(*h),
            Owned::Imu {
                t0_ns,
                dt_ns,
                imu_id,
                flags,
                seq0,
                samples,
            } => Packet::ImuBatch(ImuBatch {
                t0_ns: *t0_ns,
                dt_ns: *dt_ns,
                imu_id: *imu_id,
                flags: *flags,
                seq0: *seq0,
                samples: ImuSamples::Typed(samples),
            }),
            Owned::Baro(b) => Packet::Baro(*b),
            Owned::Mag(m) => Packet::Mag(*m),
            Owned::Trigger(t) => Packet::Trigger(*t),
            Owned::Pps(p) => Packet::Pps(*p),
            Owned::Gnss { t_ns, port, bytes } => Packet::GnssRaw(GnssRaw {
                t_ns: *t_ns,
                port: *port,
                bytes,
            }),
            Owned::Rtcm { port, bytes } => Packet::RtcmIn(RtcmIn { port: *port, bytes }),
            Owned::Config(c) => Packet::Config(*c),
            Owned::Health(h) => Packet::Health(*h),
        }
    }

    fn from_packet(p: &Packet<'_>) -> Self {
        match *p {
            Packet::Hello(h) => Owned::Hello(h),
            Packet::ImuBatch(b) => Owned::Imu {
                t0_ns: b.t0_ns,
                dt_ns: b.dt_ns,
                imu_id: b.imu_id,
                flags: b.flags,
                seq0: b.seq0,
                samples: b.samples.iter().collect(),
            },
            Packet::Baro(b) => Owned::Baro(b),
            Packet::Mag(m) => Owned::Mag(m),
            Packet::Trigger(t) => Owned::Trigger(t),
            Packet::Pps(p) => Owned::Pps(p),
            Packet::GnssRaw(g) => Owned::Gnss {
                t_ns: g.t_ns,
                port: g.port,
                bytes: g.bytes.to_vec(),
            },
            Packet::RtcmIn(r) => Owned::Rtcm {
                port: r.port,
                bytes: r.bytes.to_vec(),
            },
            Packet::Config(c) => Owned::Config(c),
            Packet::Health(h) => Owned::Health(h),
            Packet::Unknown { .. } => unreachable!("the decoder skips unknown types"),
        }
    }
}

fn sample() -> impl Strategy<Value = ImuSample> {
    (any::<[i16; 3]>(), any::<[i16; 3]>(), any::<i16>()).prop_map(|(gyro, accel, temp)| ImuSample {
        gyro,
        accel,
        temp,
    })
}

fn finite() -> impl Strategy<Value = f32> {
    -1.0e6f32..1.0e6f32
}

fn packet() -> impl Strategy<Value = Owned> {
    prop_oneof![
        (
            any::<u16>(),
            any::<[u8; 3]>(),
            any::<u16>(),
            any::<u32>(),
            any::<[u8; 12]>(),
            any::<[u8; 16]>()
        )
            .prop_map(
                |(proto_ver, fw, board_id, caps, uid, board_name)| Owned::Hello(Hello {
                    proto_ver,
                    fw_major: fw[0],
                    fw_minor: fw[1],
                    fw_patch: fw[2],
                    board_id,
                    caps,
                    uid,
                    board_name,
                })
            ),
        (
            any::<u64>(),
            any::<u32>(),
            any::<u8>(),
            any::<u8>(),
            any::<u32>(),
            prop::collection::vec(sample(), 1..=IMU_MAX_SAMPLES)
        )
            .prop_map(|(t0_ns, dt_ns, imu_id, flags, seq0, samples)| Owned::Imu {
                t0_ns,
                dt_ns,
                imu_id,
                flags,
                seq0,
                samples
            }),
        (any::<u64>(), finite(), finite(), any::<u8>()).prop_map(
            |(t_ns, pressure_pa, temp_c, baro_id)| {
                Owned::Baro(Baro {
                    t_ns,
                    pressure_pa,
                    temp_c,
                    baro_id,
                })
            }
        ),
        (
            any::<u64>(),
            proptest::array::uniform3(finite()),
            any::<u8>()
        )
            .prop_map(|(t_ns, field_ut, mag_id)| Owned::Mag(Mag {
                t_ns,
                field_ut,
                mag_id
            })),
        (
            any::<u32>(),
            any::<u8>(),
            any::<u8>(),
            any::<u64>(),
            any::<u64>(),
            any::<u32>()
        )
            .prop_map(
                |(seq, cam_mask, flags, t_start_ns, t_mid_ns, exposure_ns)| Owned::Trigger(
                    Trigger {
                        seq,
                        cam_mask,
                        flags,
                        t_start_ns,
                        t_mid_ns,
                        exposure_ns
                    }
                )
            ),
        (
            any::<u64>(),
            0u8..4,
            any::<u32>(),
            any::<i32>(),
            any::<i32>()
        )
            .prop_map(
                |(t_ns, locked, pps_count, phase_err_ns, freq_corr_ppb)| Owned::Pps(Pps {
                    t_ns,
                    locked,
                    pps_count,
                    phase_err_ns,
                    freq_corr_ppb
                })
            ),
        (
            any::<u64>(),
            any::<u8>(),
            prop::collection::vec(any::<u8>(), 0..=GNSS_MAX_BYTES)
        )
            .prop_map(|(t_ns, port, bytes)| Owned::Gnss { t_ns, port, bytes }),
        (
            any::<u8>(),
            prop::collection::vec(any::<u8>(), 0..=RTCM_MAX_BYTES)
        )
            .prop_map(|(port, bytes)| Owned::Rtcm { port, bytes }),
        (
            any::<[u16; 2]>(),
            any::<u32>(),
            any::<[u8; 5]>(),
            any::<i8>()
        )
            .prop_map(|(hz, exposure_us, b, heater)| {
                Owned::Config(Config {
                    trigger_hz: hz[0],
                    exposure_us,
                    imu_odr_hz: hz[1],
                    cam_mask: b[0],
                    trigger_mode: b[1],
                    pps_out: b[2],
                    gprmc_out: b[3],
                    imu_batch: b[4],
                    heater_target_c: heater,
                })
            }),
        (
            any::<u64>(),
            any::<u32>(),
            any::<[i16; 2]>(),
            any::<u16>(),
            any::<[u32; 4]>(),
            any::<[u8; 2]>()
        )
            .prop_map(
                |(t_ns, uptime_s, temps, vin_mv, counters, tail)| Owned::Health(Health {
                    t_ns,
                    uptime_s,
                    mcu_temp_c: temps[0],
                    imu_temp_c: temps[1],
                    vin_mv,
                    usb_tx_drops: counters[0],
                    imu_fifo_overflows: counters[1],
                    rx_crc_errors: counters[2],
                    trigger_missed: counters[3],
                    pps_state: tail[0],
                    heater_duty_pct: tail[1],
                })
            ),
    ]
}

/// Feed `wire` to a fresh decoder in chunks of the given sizes (cycled).
fn decode_chunked(wire: &[u8], chunks: &[usize]) -> (Vec<Owned>, DecodeStats) {
    let mut dec = StreamDecoder::new();
    let mut out = Vec::new();
    let mut rest = wire;
    let mut sizes = chunks.iter().cycle();
    while !rest.is_empty() {
        let n = (*sizes.next().unwrap()).clamp(1, rest.len());
        let (head, tail) = rest.split_at(n);
        dec.push(head, |p| out.push(Owned::from_packet(&p)));
        rest = tail;
    }
    (out, dec.stats())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Arbitrary packets, encoded by the protocol crate, split into arbitrary
    /// read sizes, come back out of this crate's decoder identical.
    #[test]
    fn arbitrary_packets_round_trip_through_arbitrary_chunking(
        pkts in prop::collection::vec(packet(), 1..12),
        chunks in prop::collection::vec(1usize..300, 1..16),
    ) {
        let mut wire = Vec::new();
        for p in &pkts {
            wire.extend(encode(&p.as_packet()).unwrap());
        }
        let (got, stats) = decode_chunked(&wire, &chunks);
        prop_assert_eq!(&got, &pkts);
        prop_assert_eq!(stats.corrupt(), 0);
        prop_assert_eq!(stats.packets, pkts.len() as u64);
    }

    /// Garbage between frames never yields a bogus packet past the next
    /// delimiter: every frame after it still decodes.
    #[test]
    fn leading_garbage_resyncs_on_the_next_delimiter(
        garbage in prop::collection::vec(1u8..=255, 0..64),
        pkts in prop::collection::vec(packet(), 1..6),
    ) {
        let mut wire = garbage.clone();
        wire.push(0);
        for p in &pkts {
            wire.extend(encode(&p.as_packet()).unwrap());
        }
        let (got, _) = decode_chunked(&wire, &[7]);
        prop_assert_eq!(got, pkts);
    }
}

fn trigger(seq: u32) -> Owned {
    Owned::Trigger(Trigger {
        seq,
        cam_mask: 0b11,
        flags: 0,
        t_start_ns: 1_000,
        t_mid_ns: 3_000,
        exposure_ns: 4_000,
    })
}

#[test]
fn a_corrupted_payload_byte_counts_a_crc_error_and_the_stream_resyncs() {
    // Corrupt one payload byte of the raw frame, then COBS-encode it: the
    // frame is well-formed on the wire and fails only its CRC.
    let mut raw = [0u8; MAX_RAW];
    let n = encode_raw(&trigger(2).as_packet(), &mut raw).unwrap();
    raw[3] ^= 0x5A;
    let mut bad = vec![0u8; MAX_FRAME];
    let m = cobs::encode(&raw[..n], &mut bad).unwrap();
    bad.truncate(m);
    bad.push(0);

    let mut wire = encode(&trigger(1).as_packet()).unwrap();
    wire.extend(&bad);
    wire.extend(encode(&trigger(3).as_packet()).unwrap());
    let (got, stats) = decode_chunked(&wire, &[5, 1, 64]);
    assert_eq!(got, vec![trigger(1), trigger(3)]);
    assert_eq!(stats.crc_errors, 1);
    assert_eq!(stats.framing_errors, 0);
}

#[test]
fn a_flipped_wire_byte_loses_only_its_own_frame() {
    // Corruption on the encoded wire may show up as a CRC or a framing error
    // (a code byte changed); either way it is counted and the neighbours
    // survive, provided the flip does not create or remove a delimiter.
    let first = encode(&trigger(1).as_packet()).unwrap();
    let middle = encode(&trigger(2).as_packet()).unwrap();
    for i in 0..middle.len() - 1 {
        let mut hit = middle.clone();
        hit[i] = if hit[i] == 0xFF { 0x01 } else { hit[i] + 1 };
        if hit[i] == 0 {
            continue;
        }
        let mut wire = first.clone();
        wire.extend(&hit);
        wire.extend(encode(&trigger(3).as_packet()).unwrap());
        let (got, stats) = decode_chunked(&wire, &[3]);
        assert_eq!(got.first(), Some(&trigger(1)), "byte {i}");
        assert_eq!(got.last(), Some(&trigger(3)), "byte {i}");
        assert!(
            got.len() == 2 && stats.corrupt() == 1,
            "byte {i}: {stats:?}"
        );
    }
}

#[test]
fn an_oversized_block_is_dropped_as_framing() {
    let mut wire = vec![0x42u8; MAX_FRAME + 10];
    wire.push(0);
    wire.extend(encode(&trigger(9).as_packet()).unwrap());
    let (got, stats) = decode_chunked(&wire, &[100]);
    assert_eq!(got, vec![trigger(9)]);
    assert_eq!(stats.framing_errors, 1);
}

#[test]
fn unknown_types_are_counted_and_skipped() {
    let unknown = Packet::Unknown {
        ty: 0x7E,
        payload: &[1, 2, 3],
    };
    let mut wire = encode(&unknown).unwrap();
    wire.extend(encode(&trigger(4).as_packet()).unwrap());
    let (got, stats) = decode_chunked(&wire, &[2]);
    assert_eq!(got, vec![trigger(4)]);
    assert_eq!(stats.unknown, 1);
}
