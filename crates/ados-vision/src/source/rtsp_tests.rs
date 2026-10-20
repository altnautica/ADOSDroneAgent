use super::*;
use crate::trigger::{TriggerFeed, TriggerJoiner};
use ados_sensorbus::v1::TriggerRecord;
use ados_sensorbus::Origin;

const ID: WeFrameId = WeFrameId {
    seq: 42,
    sim_time_ns: 1_234_567_890,
    cam: 1,
};

fn sei(id: &WeFrameId) -> Vec<u8> {
    let mut p = crate::sei::WE_SEI_UUID.to_vec();
    p.extend_from_slice(&id.seq.to_le_bytes());
    p.extend_from_slice(&id.sim_time_ns.to_le_bytes());
    p.extend_from_slice(&id.cam.to_le_bytes());
    p.extend_from_slice(&[0, 0, 0, 0]);
    // Left unescaped: the parser only removes `00 00 03`, so raw bytes pass.
    let mut nal = vec![0x06, 5, 40];
    nal.extend_from_slice(&p);
    nal.push(0x80);
    nal
}

#[test]
fn nothing_but_parameter_sets_reaches_the_decoder_before_the_first_idr() {
    let mut au = AccessUnits::default();
    // A P-slice picture joined mid-GOP: dropped, and no picture expected.
    assert_eq!(au.on_nal(&sei(&ID), 1), (false, None));
    assert_eq!(au.on_nal(&[0x41, 0x9a], 1), (false, None));
    assert_eq!(au.on_nal(&[0x67, 0x42], 2), (true, None));
    assert_eq!(au.on_nal(&[0x68, 0xce], 2), (true, None));
    let next = WeFrameId { seq: 43, ..ID };
    assert_eq!(au.on_nal(&sei(&next), 3), (false, None));
    assert_eq!(
        au.on_nal(&[0x65, 0x88], 3),
        (
            true,
            Some(Pending {
                id: Some(next),
                arrival_ns: 3
            })
        )
    );
    // A second slice of the same picture forwards but starts nothing; a
    // picture without an SEI carries no identity.
    assert_eq!(au.on_nal(&[0x65, 0x08], 3), (true, None));
    assert_eq!(
        au.on_nal(&[0x41, 0x9a], 4),
        (
            true,
            Some(Pending {
                id: None,
                arrival_ns: 4
            })
        )
    );
}

#[test]
fn a_frame_with_an_identity_is_on_the_sim_timebase() {
    let p = Pending {
        id: Some(ID),
        arrival_ns: 5_000,
    };
    let t = frame_timing(Some(p), 9_000, None);
    assert_eq!(t.ts_ns, 5_000);
    assert_eq!(t.ts_sensor_ns, Some(1_234_567_890));
    assert_eq!(t.trigger_seq, Some(42));
    assert_eq!(t.timebase, TIMEBASE_SIM);
    // No identity: host arrival only.
    assert_eq!(
        frame_timing(
            Some(Pending {
                id: None,
                arrival_ns: 7
            }),
            9,
            None
        ),
        FrameTiming::host(7)
    );
    assert_eq!(frame_timing(None, 9, None), FrameTiming::host(9));
}

#[test]
fn the_hub_trigger_of_the_same_sequence_sets_the_exposure_midpoint() {
    let feed = TriggerFeed::default();
    feed.ingest(
        Origin::Hub,
        "cam1",
        TriggerRecord {
            seq: 42,
            t_mid_ns: 4_200,
            exposure_ns: 2_000_000,
            gain: 0.0,
        },
    );
    let mut cam = CameraTriggers::new(
        feed,
        (Origin::Hub, "cam1".to_string()),
        TriggerJoiner::same_sequence(),
    );
    let p = Pending {
        id: Some(ID),
        arrival_ns: 90_000_000,
    };
    let t = frame_timing(Some(p), 0, Some(&mut cam));
    assert_eq!(t.ts_ns, 4_200);
    assert_eq!(t.exposure_ns, Some(2_000_000));
    assert_eq!(t.trigger_seq, Some(42));
}

#[test]
fn bayer_is_refused_for_a_stream() {
    assert!(check_format(FrameFormat::BayerRggb8).is_err());
    assert!(check_format(FrameFormat::BayerRggb16).is_err());
    for ok in [FrameFormat::Rgb24, FrameFormat::Nv12, FrameFormat::Gray8] {
        assert!(check_format(ok).is_ok());
    }
}

/// The decoder emits one frame per picture fed to it, from the first one:
/// a decoder that drops the pictures it probes would pair every later frame
/// with an identity that many pictures older. Runs the real `ffmpeg` (the
/// agent's runtime dependency) on a 60-picture stream it encodes first.
#[cfg(target_os = "linux")]
#[test]
fn the_decoder_emits_every_picture_it_is_fed() {
    use std::io::{Read, Write};
    let encoded = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .args(["testsrc=size=320x200:rate=20", "-frames:v", "60"])
        .args(["-c:v", "libx264", "-bf", "0", "-g", "20", "-f", "h264", "-"])
        .output()
        .expect("ffmpeg is installed");
    assert!(encoded.status.success() && !encoded.stdout.is_empty());
    let settings = RtspSettings {
        url: "rtsp://unused".into(),
        width: 320,
        height: 200,
        format: FrameFormat::Gray8,
    };
    let mut decode = std::process::Command::new("ffmpeg")
        .args(decoder_args(&settings))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the decoder");
    let mut stdin = decode.stdin.take().unwrap();
    let input = encoded.stdout;
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let mut raw = Vec::new();
    decode.stdout.take().unwrap().read_to_end(&mut raw).unwrap();
    feeder.join().unwrap().unwrap();
    assert!(decode.wait().unwrap().success());
    assert_eq!(raw.len(), 60 * 320 * 200, "one gray frame per picture");
}

fn pending(seq: u64) -> Pending {
    Pending {
        id: Some(WeFrameId { seq, ..ID }),
        arrival_ns: seq,
    }
}

#[test]
fn decoded_frames_pair_with_pictures_in_order() {
    let mut p = Pairing::default();
    for s in 0..3 {
        p.push(pending(s));
    }
    let seqs: Vec<u64> = (0..3).map(|_| p.pop().unwrap().id.unwrap().seq).collect();
    assert_eq!(seqs, [0, 1, 2]);
}

#[test]
fn an_overflow_never_pairs_a_frame_with_a_later_identity() {
    let mut p = Pairing::default();
    for s in 0..=MAX_PENDING as u64 {
        p.push(pending(s));
    }
    // The decoder fell behind. Frames already in flight would come out
    // carrying pictures 0, 1, ...; clearing the queue would have paired them
    // with whatever arrived next. Instead every frame is refused until the
    // stream restarts.
    p.push(pending(1000));
    assert_eq!(p.pop(), Err(PairingLost));
    assert_eq!(p.pop(), Err(PairingLost));
    // The restarted stream starts a fresh pairing, aligned from its first IDR.
    let mut fresh = Pairing::default();
    fresh.push(pending(2000));
    assert_eq!(fresh.pop().unwrap().id.unwrap().seq, 2000);
}

#[test]
fn a_frame_no_picture_accounts_for_loses_the_pairing() {
    let mut p = Pairing::default();
    assert_eq!(p.pop(), Err(PairingLost));
    p.push(pending(1));
    assert_eq!(p.pop(), Err(PairingLost));
}

#[tokio::test]
async fn the_sim_clock_publishes_sync_pairs_on_the_bus() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sensors.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let clock = SimClock::publishing(Publisher::spawn(path, Origin::Vision));
    let (mut conn, _) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            clock.observe(10, 20);
            if let Ok(Ok(c)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept()).await
            {
                break c;
            }
        }
    })
    .await
    .unwrap();
    clock.observe(1_000, 2_000);
    let mut buf = Vec::new();
    let frame = ados_sensorbus::client::read_frame(&mut conn, &mut buf)
        .await
        .unwrap();
    assert_eq!(frame.kind, Kind::Sync);
    assert_eq!(frame.src, "sim");
    let obs: SyncObs = frame.decode_msgpack().unwrap();
    assert_eq!(obs.source, SyncSource::Sim);
    assert!(obs.source_ns == 2_000 || obs.source_ns == 20);
}
