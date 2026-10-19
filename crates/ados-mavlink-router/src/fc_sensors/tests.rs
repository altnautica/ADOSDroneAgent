use ados_protocol::mavlink::ardupilotmega::{
    MavMessage, CAMERA_FEEDBACK_DATA, HIGHRES_IMU_DATA, RAW_IMU_DATA, SCALED_PRESSURE_DATA,
    SYSTEM_TIME_DATA, TIMESYNC_DATA,
};
use ados_sensorbus::v1::TriggerRecord;
use ados_sensorbus::SyncSource;

use super::convert::{self, frd_to_flu, Unwrapper, GRAVITY_MSS};
use super::lane::{BusOut, LaneCore, IMU_SRC, RAW_IMU_SRC};
use super::rtcm::RtcmInjector;
use super::timesync::{Reply, TimesyncTracker};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

fn timesync(tc1: i64, ts1: i64) -> MavMessage {
    MavMessage::TIMESYNC(TIMESYNC_DATA { tc1, ts1 })
}

fn feedback(time_usec: u64, img_idx: u16) -> MavMessage {
    MavMessage::CAMERA_FEEDBACK(CAMERA_FEEDBACK_DATA {
        time_usec,
        img_idx,
        ..Default::default()
    })
}

fn highres(time_usec: u64) -> MavMessage {
    MavMessage::HIGHRES_IMU(HIGHRES_IMU_DATA {
        time_usec,
        xacc: 0.5,
        zacc: -9.8,
        ..Default::default()
    })
}

#[test]
fn frd_to_flu_flips_y_and_z() {
    assert_eq!(frd_to_flu([1.0, 2.0, 3.0]), [1.0, -2.0, -3.0]);
}

#[test]
fn highres_imu_converts_axes_for_gyro_and_accel() {
    let s = convert::highres_sample(&HIGHRES_IMU_DATA {
        xacc: 1.0,
        yacc: 2.0,
        zacc: -9.75,
        xgyro: 0.1,
        ygyro: 0.2,
        zgyro: -0.3,
        temperature: 41.5,
        ..Default::default()
    });
    let want_accel = [1.0, -2.0, 9.75];
    let want_gyro = [0.1, -0.2, 0.3];
    for i in 0..3 {
        assert!(close(s.accel[i], want_accel[i]), "accel {i}: {:?}", s.accel);
        assert!(close(s.gyro[i], want_gyro[i]), "gyro {i}: {:?}", s.gyro);
    }
    assert_eq!(s.temp_c, Some(41.5));
}

#[test]
fn raw_imu_scales_ardupilot_units_to_si_flu() {
    // ArduPilot RAW_IMU: accel milli-g, gyro mrad/s.
    let s = convert::raw_imu_sample(&RAW_IMU_DATA {
        xacc: 100,
        yacc: -250,
        zacc: -1000,
        xgyro: 1500,
        ygyro: 500,
        zgyro: -20,
        ..Default::default()
    });
    let want_accel = [0.1 * GRAVITY_MSS, 0.25 * GRAVITY_MSS, GRAVITY_MSS];
    let want_gyro = [1.5, -0.5, 0.02];
    for i in 0..3 {
        assert!(close(s.accel[i], want_accel[i]), "accel {i}: {:?}", s.accel);
        assert!(close(s.gyro[i], want_gyro[i]), "gyro {i}: {:?}", s.gyro);
    }
    assert_eq!(s.temp_c, None);
}

#[test]
fn scaled_pressure_becomes_pascal_baro() {
    let b = convert::baro(
        &SCALED_PRESSURE_DATA {
            press_abs: 1013.25,
            temperature: 2150,
            ..Default::default()
        },
        7,
    );
    assert_eq!(b.t_ns, 7);
    assert!(close(b.pressure_pa, 101_325.0));
    assert!((b.temp_c - 21.5).abs() < 1e-6);
}

#[test]
fn unwrapper_extends_wrapping_counters() {
    let mut u = Unwrapper::new(16);
    assert_eq!(u.unwrap(65_534), 65_534);
    assert_eq!(u.unwrap(65_535), 65_535);
    assert_eq!(u.unwrap(0), 65_536);
    assert_eq!(u.unwrap(3), 65_539);
    // A small step back is reordering, not a wrap.
    assert_eq!(u.unwrap(1), 65_537);
    u.reset();
    assert_eq!(u.unwrap(5), 5);
}

#[test]
fn timesync_matches_only_our_requests() {
    let mut t = TimesyncTracker::default();
    t.sent(1_000_000_000);
    // A request (tc1 = 0) and another station's reply are not ours.
    assert_eq!(
        t.on_timesync(0, 1_000_000_000, 1_000_200_000),
        Reply::Ignored
    );
    assert_eq!(t.on_timesync(9, 42, 1_000_200_000), Reply::Ignored);
    let Reply::Accepted(obs) = t.on_timesync(5_000_000_000, 1_000_000_000, 1_000_200_000) else {
        panic!("our reply must be accepted");
    };
    assert_eq!(obs.source, SyncSource::Fc);
    assert_eq!(obs.host_ns, 1_000_100_000);
    assert_eq!(obs.source_ns, 5_000_000_000);
    assert_eq!(obs.rtt_ns, 200_000);
    // A request matches once.
    assert_eq!(
        t.on_timesync(5_000_000_000, 1_000_000_000, 1_000_300_000),
        Reply::Ignored
    );
    assert_eq!(t.to_host(5_000_000_000), Some(1_000_100_000));
}

#[test]
fn timesync_resets_when_the_fc_clock_jumps_back() {
    let mut t = TimesyncTracker::default();
    t.sent(1_000_000_000);
    assert!(matches!(
        t.on_timesync(9_000_000_000, 1_000_000_000, 1_000_200_000),
        Reply::Accepted(_)
    ));
    assert_eq!(t.epoch(), 0);
    // The FC rebooted: its clock reads small again. The old relation is gone
    // and the new exchange alone defines the fit.
    t.sent(2_000_000_000);
    let Reply::Accepted(obs) = t.on_timesync(1_000_000, 2_000_000_000, 2_000_200_000) else {
        panic!("first exchange after a reset is accepted");
    };
    assert_eq!(t.epoch(), 1);
    assert_eq!(obs.source_ns, 1_000_000);
    assert_eq!(t.to_host(1_000_000), Some(2_000_100_000));
}

/// A lane whose TIMESYNC fit maps FC boot ns `5e9` to host ns `1.0001e9`.
fn fitted_lane() -> LaneCore {
    let mut lane = LaneCore::default();
    let mut out = Vec::new();
    lane.timesync_sent(1_000_000_000);
    lane.on_message(
        1_000_200_000,
        &timesync(5_000_000_000, 1_000_000_000),
        &mut out,
    );
    assert!(matches!(out.as_slice(), [BusOut::Sync(_)]));
    lane
}

#[test]
fn camera_feedback_becomes_trigger_record() {
    let mut out = Vec::new();
    // No relation yet: the receive time stands in.
    let mut lane = LaneCore::default();
    lane.on_message(777, &feedback(5_000_050, 12), &mut out);
    assert_eq!(out.pop(), Some(BusOut::Trigger(convert::trigger(12, 777))));

    let mut lane = fitted_lane();
    // Boot-time stamp 50 µs after the fitted point.
    lane.on_message(1_000_160_000, &feedback(5_000_050, 65_535), &mut out);
    let want = TriggerRecord {
        seq: 65_535,
        t_mid_ns: 1_000_150_000,
        exposure_ns: 0,
        gain: 0.0,
    };
    assert_eq!(out.pop(), Some(BusOut::Trigger(want)));
    // img_idx wraps at 16 bits; seq keeps counting. An epoch stamp maps back
    // through SYSTEM_TIME.
    let unix_at_boot = 1_700_000_000_000_000u64;
    lane.on_message(
        1_000_170_000,
        &MavMessage::SYSTEM_TIME(SYSTEM_TIME_DATA {
            time_unix_usec: unix_at_boot + 5_000_000,
            time_boot_ms: 5_000,
        }),
        &mut out,
    );
    assert!(out.is_empty());
    lane.on_message(
        1_000_180_000,
        &feedback(unix_at_boot + 5_000_060, 0),
        &mut out,
    );
    let Some(BusOut::Trigger(t)) = out.pop() else {
        panic!("trigger expected");
    };
    assert_eq!(t.seq, 65_536);
    assert_eq!(t.t_mid_ns, 1_000_160_000);
}

#[test]
fn implausible_converted_time_falls_back_to_receipt() {
    let mut lane = fitted_lane();
    let mut out = Vec::new();
    // An FC time that maps far into the past (a reboot TIMESYNC has not seen
    // yet) is not trusted.
    lane.on_message(1_000_300_000, &feedback(1_000, 1), &mut out);
    assert_eq!(
        out.pop(),
        Some(BusOut::Trigger(convert::trigger(1, 1_000_300_000)))
    );
}

#[test]
fn imu_samples_batch_per_ten_ms_with_running_seq() {
    let mut lane = fitted_lane();
    let mut out = Vec::new();
    // 4 ms apart on the FC clock and on receipt.
    for i in 0..3u64 {
        let rx = 1_000_200_000 + i * 4_000_000;
        lane.on_message(rx, &highres(5_000_100 + i * 4_000), &mut out);
    }
    assert!(out.is_empty(), "under 10 ms: still batching");
    lane.on_message(1_012_200_000, &highres(5_012_100), &mut out);
    let Some(BusOut::Imu { src, batch }) = out.pop() else {
        panic!("batch expected");
    };
    assert_eq!(src, IMU_SRC);
    assert_eq!(batch.samples.len(), 4);
    let seqs: Vec<u64> = batch.samples.iter().map(|s| s.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3]);
    assert_eq!(batch.samples[0].t_ns, 1_000_200_000);
    assert_eq!(batch.samples[3].t_ns, 1_012_200_000);
    // FRD -> FLU on the way: z accel up is positive.
    assert!(close(batch.samples[0].accel[2], 9.8));

    // A RAW_IMU sample is its own stream: the pending batch flushes first.
    lane.on_message(1_013_200_000, &highres(5_013_100), &mut out);
    lane.on_message(
        1_014_200_000,
        &MavMessage::RAW_IMU(RAW_IMU_DATA::default()),
        &mut out,
    );
    assert!(matches!(&out[..], [BusOut::Imu { src: IMU_SRC, batch }] if batch.samples[0].seq == 4));
    out.clear();
    lane.flush_due(1_024_200_000, &mut out);
    let Some(BusOut::Imu { src, batch }) = out.pop() else {
        panic!("timer flush expected");
    };
    assert_eq!(src, RAW_IMU_SRC);
    assert_eq!(batch.samples[0].seq, 0);
}

/// A CRC-valid RTCM3 frame of `total_len` bytes.
fn rtcm3(total_len: usize) -> Vec<u8> {
    let payload: Vec<u8> = (0..total_len - 6).map(|i| i as u8).collect();
    ados_sensorbus::rtcm::encode_rtcm3(&payload)
}

#[test]
fn a_400_byte_rtcm_frame_becomes_three_gps_rtcm_data_messages() {
    let mut inj = RtcmInjector::default();
    let frame = rtcm3(400);
    let msgs = inj.messages("gnss0", &frame);
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        msgs.iter().map(|m| m.len).collect::<Vec<_>>(),
        vec![180, 180, 40]
    );
    for (id, m) in msgs.iter().enumerate() {
        assert_eq!(m.flags, 0x01 | (id as u8) << 1, "fragment {id}, sequence 0");
    }
    assert_eq!(&msgs[0].data[..180], &frame[..180]);
    assert_eq!(&msgs[2].data[..40], &frame[360..]);
    // The next message advances the sequence.
    let next = inj.messages("gnss0", &frame);
    for (id, m) in next.iter().enumerate() {
        assert_eq!(m.flags, 0x01 | (id as u8) << 1 | 1 << 3);
    }
    // Two frames in one payload, the short one unfragmented.
    let mut both = rtcm3(30);
    both.extend(rtcm3(400));
    let msgs = inj.messages("gnss0", &both);
    assert_eq!(msgs.len(), 4);
    assert_eq!((msgs[0].flags, msgs[0].len), (0, 30));
    assert_eq!(msgs[1].flags, 0x01 | 2 << 3);
    // A 360-byte frame ends with a zero-length terminator; junk is skipped.
    let mut junk = vec![0x00, 0x11];
    junk.extend(rtcm3(360));
    let msgs = inj.messages("gnss0", &junk);
    assert_eq!(
        msgs.iter().map(|m| m.len).collect::<Vec<_>>(),
        vec![180, 180, 0]
    );
    assert!(inj.messages("rest", &[0xD3, 0, 1, 7, 0, 0, 0]).is_empty());
}

#[test]
fn a_message_split_across_bus_frames_is_injected_once_whole() {
    let frame = rtcm3(400);
    for cut in 1..frame.len() {
        let mut inj = RtcmInjector::default();
        assert!(inj.messages("gnss0", &frame[..cut]).is_empty(), "cut {cut}");
        // Another source in between does not disturb the partial frame.
        assert_eq!(inj.messages("rest", &rtcm3(30)).len(), 1);
        let msgs = inj.messages("gnss0", &frame[cut..]);
        assert_eq!(msgs.len(), 3, "cut {cut}");
        assert_eq!(&msgs[2].data[..40], &frame[360..]);
    }
    // A gap in the stream drops the partial frame rather than splicing it.
    let mut inj = RtcmInjector::default();
    inj.messages("gnss0", &frame[..100]);
    inj.reset_streams();
    assert!(inj.messages("gnss0", &frame[100..]).is_empty());
}
