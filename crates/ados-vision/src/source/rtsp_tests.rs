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
fn pixel_formats_map_to_ffmpeg_tokens() {
    assert_eq!(ffmpeg_pix_fmt(FrameFormat::Rgb24), "rgb24");
    assert_eq!(ffmpeg_pix_fmt(FrameFormat::Gray8), "gray");
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
