use super::*;
use ados_protocol::logd::{EventFrame, LogFrame, TelemetryFrame};

#[test]
fn frame_class_maps_each_variant() {
    let log = IngestFrame::Log(LogFrame::new(1, "s", Level::Info, "m"));
    let tele = IngestFrame::Telemetry(TelemetryFrame::new(1, "m", 1.0));
    let evt = IngestFrame::Event(EventFrame::new(1, "k", "s", Level::Info));
    let hw = IngestFrame::Hw(ados_protocol::logd::HwSnapshot::new(1));
    assert_eq!(FrameClass::of(&log), FrameClass::Log);
    assert_eq!(FrameClass::of(&tele), FrameClass::Telemetry);
    assert_eq!(FrameClass::of(&evt), FrameClass::Event);
    assert_eq!(FrameClass::of(&hw), FrameClass::Hw);
}

#[test]
fn high_severity_keeps_events_and_warn_plus_logs() {
    // Events and WARN/ERROR logs are preserved through backpressure.
    assert!(is_high_severity(&IngestFrame::Event(EventFrame::new(
        1,
        "k",
        "s",
        Level::Trace
    ))));
    assert!(is_high_severity(&IngestFrame::Log(LogFrame::new(
        1,
        "s",
        Level::Warn,
        "m"
    ))));
    assert!(is_high_severity(&IngestFrame::Log(LogFrame::new(
        1,
        "s",
        Level::Error,
        "m"
    ))));
    // INFO/DEBUG/TRACE logs and all telemetry/hw are droppable.
    assert!(!is_high_severity(&IngestFrame::Log(LogFrame::new(
        1,
        "s",
        Level::Info,
        "m"
    ))));
    assert!(!is_high_severity(&IngestFrame::Log(LogFrame::new(
        1,
        "s",
        Level::Debug,
        "m"
    ))));
    assert!(!is_high_severity(&IngestFrame::Telemetry(
        TelemetryFrame::new(1, "m", 1.0)
    )));
    assert!(!is_high_severity(&IngestFrame::Hw(
        ados_protocol::logd::HwSnapshot::new(1)
    )));
}

#[tokio::test]
async fn forward_drops_low_severity_when_channel_is_full() {
    // A one-slot channel: fill it, then a droppable frame is shed and the
    // drop counter for its class advances; the producer never blocks.
    let (tx, _rx) = mpsc::channel::<IngestFrame>(1);
    let stats = IngestStats::default();

    // First send fills the only slot.
    forward(
        &tx,
        &stats,
        IngestFrame::Telemetry(TelemetryFrame::new(1, "cpu.load", 1.0)),
    )
    .await
    .unwrap();
    assert_eq!(stats.accepted(), 1);

    // Second send finds the channel full: a telemetry frame is dropped.
    forward(
        &tx,
        &stats,
        IngestFrame::Telemetry(TelemetryFrame::new(2, "cpu.load", 2.0)),
    )
    .await
    .unwrap();
    assert_eq!(stats.accepted(), 1);
    assert_eq!(stats.dropped(FrameClass::Telemetry), 1);
}

#[tokio::test]
async fn forward_drops_high_severity_only_after_the_bounded_wait() {
    // With no reader draining, a full one-slot channel forces even a
    // high-severity frame to drop, but only after the bounded wait; it is
    // never lost silently (the drop counter records it).
    let (tx, _rx) = mpsc::channel::<IngestFrame>(1);
    let stats = IngestStats::default();
    forward(
        &tx,
        &stats,
        IngestFrame::Event(EventFrame::new(1, "k", "s", Level::Error)),
    )
    .await
    .unwrap();
    assert_eq!(stats.accepted(), 1);

    forward(
        &tx,
        &stats,
        IngestFrame::Event(EventFrame::new(2, "k", "s", Level::Error)),
    )
    .await
    .unwrap();
    assert_eq!(stats.accepted(), 1);
    assert_eq!(stats.dropped(FrameClass::Event), 1);
}

#[tokio::test]
async fn forward_signals_closed_channel() {
    let (tx, rx) = mpsc::channel::<IngestFrame>(1);
    drop(rx);
    let r = forward(
        &tx,
        &IngestStats::default(),
        IngestFrame::Log(LogFrame::new(1, "s", Level::Info, "m")),
    )
    .await;
    assert!(r.is_err());
}

#[test]
fn dropped_by_class_snapshot_has_all_classes() {
    let stats = IngestStats::default();
    stats.record_dropped(FrameClass::Log);
    let m = stats.dropped_by_class();
    assert_eq!(m.get("log"), Some(&1));
    assert_eq!(m.get("telemetry"), Some(&0));
    assert_eq!(m.get("event"), Some(&0));
    assert_eq!(m.get("hw"), Some(&0));
}

/// Drive `serve_client` over a socket pair with the given frames on the
/// wire, returning what reached the writer channel and the stats.
async fn serve_wire(wire: Vec<u8>, operator: bool) -> (Vec<IngestFrame>, Arc<IngestStats>) {
    use tokio::io::AsyncWriteExt;
    let (mut producer, server) = UnixStream::pair().unwrap();
    let (tx, mut rx) = mpsc::channel::<IngestFrame>(16);
    let stats = Arc::new(IngestStats::default());
    let task = tokio::spawn(serve_client(server, tx, Arc::clone(&stats), operator));
    producer.write_all(&wire).await.unwrap();
    drop(producer);
    task.await.unwrap();
    let mut got = Vec::new();
    while let Ok(f) = rx.try_recv() {
        got.push(f);
    }
    (got, stats)
}

/// A plugin-plane peer cannot write under a core service's name: its log
/// and event sources are namespaced, as are its metric and signal keys.
#[tokio::test]
async fn a_plugin_peer_cannot_write_as_a_core_service() {
    let mut hw = ados_protocol::logd::HwSnapshot::new(4);
    hw.signals.insert(
        "thermal.soc_c".into(),
        ados_protocol::logd::Value::from(99.0),
    );
    let frames = [
        IngestFrame::Log(LogFrame::new(1, "ados-mavlink-router", Level::Error, "m")),
        IngestFrame::Event(EventFrame::new(
            2,
            "fc.disarm",
            "ados-mavlink-router",
            Level::Warn,
        )),
        IngestFrame::Telemetry(TelemetryFrame::new(3, "cpu.load", 1.0)),
        IngestFrame::Hw(hw),
    ];
    let wire: Vec<u8> = frames.iter().flat_map(|f| f.encode().unwrap()).collect();

    let (got, _) = serve_wire(wire.clone(), false).await;
    assert_eq!(got.len(), 4);
    match (&got[0], &got[1], &got[2], &got[3]) {
        (
            IngestFrame::Log(l),
            IngestFrame::Event(e),
            IngestFrame::Telemetry(t),
            IngestFrame::Hw(h),
        ) => {
            assert_eq!(l.source, "plugin:ados-mavlink-router");
            assert_eq!(e.source, "plugin:ados-mavlink-router");
            assert_eq!(t.metric, "plugin.cpu.load");
            assert!(h.signals.contains_key("plugin.thermal.soc_c"));
            assert!(!h.signals.contains_key("thermal.soc_c"));
        }
        other => panic!("unexpected frames {other:?}"),
    }

    // An operator-plane peer's records are stored as written.
    let (got, _) = serve_wire(wire, true).await;
    match &got[0] {
        IngestFrame::Log(l) => assert_eq!(l.source, "ados-mavlink-router"),
        other => panic!("unexpected frame {other:?}"),
    }
}

/// A plugin holding `sensor.stream.read` carries the sensors reader group as a
/// supplementary group and nothing on the operator plane. The decision
/// `is_operator_peer` makes on its credentials keeps it off the operator
/// plane, so what it writes on the ingest socket stays `plugin:`-attributed.
#[tokio::test]
async fn a_sensor_bus_reader_stays_plugin_attributed() {
    const SERVER_UID: u32 = 0;
    const OPERATOR_GID: u32 = 988;
    const SENSORS_GID: u32 = 977;
    const PLUGINS_GID: u32 = 966;
    let operator = ados_protocol::ipc::operator_peer_allowed(
        61_001,
        &[PLUGINS_GID, SENSORS_GID],
        SERVER_UID,
        Some(OPERATOR_GID),
    );
    assert!(!operator);
    let frame = IngestFrame::Log(LogFrame::new(1, "ados-clock", Level::Error, "m"));
    let (got, _) = serve_wire(frame.encode().unwrap(), operator).await;
    match &got[..] {
        [IngestFrame::Log(l)] => assert_eq!(l.source, "plugin:ados-clock"),
        other => panic!("unexpected frames {other:?}"),
    }
    // The same peer with the operator group would be the operator.
    assert!(ados_protocol::ipc::operator_peer_allowed(
        61_001,
        &[PLUGINS_GID, SENSORS_GID, OPERATOR_GID],
        SERVER_UID,
        Some(OPERATOR_GID),
    ));
}

/// A frame that arrives whole but does not decode is skipped; the frames
/// the producer batched behind it still reach the writer.
#[tokio::test]
async fn an_undecodable_frame_is_skipped_not_fatal() {
    let mut future = LogFrame::new(1, "s", Level::Info, "from a newer producer");
    future.v = 99;
    let mut wire = IngestFrame::Log(future).encode().unwrap();
    wire.extend(ados_protocol::frame::encode_frame(b"\xc1not msgpack", LOGD_MAX_FRAME).unwrap());
    wire.extend(
        IngestFrame::Log(LogFrame::new(2, "s", Level::Info, "after"))
            .encode()
            .unwrap(),
    );

    let (got, stats) = serve_wire(wire, true).await;
    assert_eq!(stats.undecodable(), 2);
    assert_eq!(got.len(), 1);
    match &got[0] {
        IngestFrame::Log(l) => assert_eq!(l.msg, "after"),
        other => panic!("unexpected frame {other:?}"),
    }
}
