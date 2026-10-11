use super::*;
use ados_protocol::logd::{EventFrame, HwSnapshot, LogFrame, TelemetryFrame};

fn filters(query: &str) -> QueryFilters {
    QueryFilters::parse(&super::super::params::QueryParams::parse(query), 0).unwrap()
}

#[test]
fn tail_slots_cap_and_free() {
    let slots = Arc::new(TailSlots::default());
    let mut guards = Vec::new();
    for _ in 0..MAX_TAIL_SUBSCRIBERS {
        guards.push(slots.try_acquire().expect("under the cap"));
    }
    assert_eq!(slots.active(), MAX_TAIL_SUBSCRIBERS);
    // The next one over the cap is refused.
    assert!(slots.try_acquire().is_none());
    // Dropping one frees a slot.
    guards.pop();
    assert_eq!(slots.active(), MAX_TAIL_SUBSCRIBERS - 1);
    assert!(slots.try_acquire().is_some());
}

#[test]
fn export_slots_cap_and_free() {
    let slots = Arc::new(ExportSlots::default());
    let mut guards = Vec::new();
    for _ in 0..MAX_CONCURRENT_EXPORTS {
        guards.push(slots.try_acquire().expect("under the cap"));
    }
    assert_eq!(slots.active(), MAX_CONCURRENT_EXPORTS);
    // The next one over the cap is refused.
    assert!(slots.try_acquire().is_none());
    // Dropping one frees a slot.
    guards.pop();
    assert_eq!(slots.active(), MAX_CONCURRENT_EXPORTS - 1);
    assert!(slots.try_acquire().is_some());
}

#[test]
fn log_filters_match_by_table_source_level_text() {
    let frame = {
        let mut l = LogFrame::new(1, "ados-video", Level::Warn, "encoder stalled");
        l.target = Some("video::encode".to_string());
        IngestFrame::Log(l)
    };
    assert!(frame_matches(&frame, &filters("kind=logs")));
    assert!(frame_matches(
        &frame,
        &filters("kind=logs&source=ados-video&level=info&text=stalled")
    ));
    // Wrong source.
    assert!(!frame_matches(&frame, &filters("kind=logs&source=api")));
    // Level floor above the frame.
    assert!(!frame_matches(&frame, &filters("kind=logs&level=error")));
    // Text not present.
    assert!(!frame_matches(&frame, &filters("kind=logs&text=nope")));
    // A log frame does not match an events tail.
    assert!(!frame_matches(&frame, &filters("kind=events")));
}

#[test]
fn event_metric_and_hw_filters() {
    let evt = IngestFrame::Event(EventFrame::new(2, "radio.lock", "ados-radio", Level::Info));
    assert!(frame_matches(
        &evt,
        &filters("kind=events&event_kind=radio.lock")
    ));
    assert!(!frame_matches(
        &evt,
        &filters("kind=events&event_kind=radio.unlock")
    ));

    let m = IngestFrame::Telemetry(TelemetryFrame::new(3, "cpu.load", 0.5));
    assert!(frame_matches(&m, &filters("kind=metrics&metric=cpu.load")));
    assert!(!frame_matches(&m, &filters("kind=metrics&metric=mem.used")));

    let hw = IngestFrame::Hw(HwSnapshot::new(4));
    assert!(frame_matches(&hw, &filters("kind=hw")));
}

#[test]
fn frame_to_json_carries_the_row_shape() {
    let mut l = LogFrame::new(10, "api", Level::Error, "boom");
    l.fields
        .insert("attempt".to_string(), rmpv::Value::from(2u64));
    let j = frame_to_json(&IngestFrame::Log(l)).unwrap();
    assert_eq!(j["kind"], "log");
    assert_eq!(j["level"], "error");
    assert_eq!(j["msg"], "boom");
    assert_eq!(j["fields"]["attempt"], 2);
}
