use super::*;
use ados_sensorbus::{v1, Baro, Origin};

const MS: u64 = 1_000_000;
const T0: u64 = 100_000_000_000;

fn imu_frame(src: &str, t: &[u64]) -> BusFrame {
    let batch = v1::ImuBatch {
        samples: t
            .iter()
            .enumerate()
            .map(|(i, &t_ns)| v1::ImuSample {
                t_ns,
                seq: i as u64,
                gyro: vec![0.1, 0.2, 0.3],
                accel: vec![0.0, 0.0, 9.81],
                temp_c: Some(40.0),
            })
            .collect(),
    };
    BusFrame::proto(Kind::Imu, src, Origin::Hub, &batch)
}

#[test]
fn imu_batches_count_samples_and_keep_the_newest_time() {
    let mut t = StreamTable::new();
    assert!(t.ingest(&imu_frame("imu0", &[T0 - 2 * MS, T0 - MS, T0]), T0 + MS));
    let r = t.report(T0 + MS);
    assert_eq!(r.streams.len(), 1);
    let s = &r.streams[0];
    assert_eq!((s.kind, s.src.as_str(), s.origin), ("imu", "imu0", "hub"));
    assert_eq!(s.count, 3);
    assert_eq!(s.last_t_ns, T0);
    assert_eq!(s.last_rx_ns, T0 + MS);
    // One frame: no span to measure a rate over.
    assert_eq!(s.rate_hz, 0.0);
}

#[test]
fn rate_over_the_window() {
    let mut t = StreamTable::new();
    // 100 Hz frames of 10 samples each for 3 s: 1000 samples/s.
    for i in 0..300u64 {
        let rx = T0 + i * 10 * MS;
        let times: Vec<u64> = (0..10).map(|k| rx - (9 - k) * MS).collect();
        t.ingest(&imu_frame("imu0", &times), rx);
    }
    let now = T0 + 299 * 10 * MS;
    let rate = t.report(now).streams[0].rate_hz;
    assert!((rate - 1000.0).abs() < 1.0, "rate {rate}");
    // A stream that stopped falls to zero once the window has passed.
    assert_eq!(t.report(now + 2_100 * MS).streams[0].rate_hz, 0.0);
}

#[test]
fn single_sample_kinds_and_their_times() {
    let mut t = StreamTable::new();
    let baro = Baro {
        t_ns: T0 + 5,
        pressure_pa: 101_325.0,
        temp_c: 20.0,
    };
    let frame = BusFrame::msgpack(Kind::Baro, "baro0", Origin::Hub, &baro).unwrap();
    assert!(t.ingest(&frame, T0 + 10));
    let gnss = v1::GnssRaw {
        t_ns: T0 + 6,
        bytes: vec![0xB5, 0x62, 1, 2, 3],
    };
    assert!(t.ingest(
        &BusFrame::proto(Kind::Gnss, "gnss0", Origin::Gnss, &gnss),
        T0 + 11
    ));
    let trig = v1::TriggerRecord {
        seq: 4,
        t_mid_ns: T0 + 7,
        exposure_ns: 4_000_000,
        gain: 1.0,
    };
    assert!(t.ingest(
        &BusFrame::proto(Kind::Trigger, "cam0", Origin::Fc, &trig),
        T0 + 12
    ));
    // Kinds that are not data streams are not listed.
    assert!(!t.ingest(
        &BusFrame::new(Kind::Rtcm, "rest", Origin::Host, vec![0xD3]),
        T0
    ));
    let sync = BusFrame::new(Kind::Sync, "hub", Origin::Hub, vec![]);
    assert!(!t.ingest(&sync, T0));
    // A payload that does not decode is dropped.
    assert!(!t.ingest(
        &BusFrame::new(Kind::Baro, "baro0", Origin::Hub, vec![0xC1]),
        T0
    ));

    let r = t.report(T0 + 20);
    let rows: Vec<(&str, &str, &str, u64, u64)> = r
        .streams
        .iter()
        .map(|s| (s.kind, s.src.as_str(), s.origin, s.last_t_ns, s.count))
        .collect();
    assert_eq!(
        rows,
        [
            ("baro", "baro0", "hub", T0 + 5, 1),
            ("gnss", "gnss0", "gnss", T0 + 6, 1),
            ("trigger", "cam0", "fc", T0 + 7, 1),
        ]
    );
}

#[test]
fn the_same_src_from_two_origins_is_two_streams() {
    let mut t = StreamTable::new();
    t.ingest(&imu_frame("imu0", &[T0]), T0);
    let mut fc = imu_frame("imu0", &[T0]);
    fc.origin = Origin::Fc;
    t.ingest(&fc, T0);
    assert_eq!(t.len(), 2);
}

#[test]
fn sensors_document_shape() {
    let mut t = StreamTable::new();
    t.ingest(&imu_frame("imu0", &[T0]), T0);
    let v = serde_json::to_value(t.report(T0 + 1)).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["streams", "updated_ns", "version"]);
    assert_eq!(v["version"], 1);
    assert_eq!(v["updated_ns"], T0 + 1);
    let mut row: Vec<&str> = v["streams"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    row.sort_unstable();
    assert_eq!(
        row,
        [
            "count",
            "kind",
            "last_rx_ns",
            "last_t_ns",
            "origin",
            "rate_hz",
            "src"
        ]
    );
    // The first key written is the version.
    let text = serde_json::to_string(&t.report(T0)).unwrap();
    assert!(text.starts_with("{\"version\":1,"), "{text}");
}
