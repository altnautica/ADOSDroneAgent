//! What the engine reads off the plugin event export (seam K), on one
//! connection: Navigation's planar scan for the depth fit (while a depth
//! model runs) and the follow beacon for the subject estimate (while the
//! tracker runs).

use std::sync::Arc;

use ados_protocol::follow_beacon::FOLLOW_BEACON_TOPIC;
use ados_protocol::shutdown::Shutdown;
use tokio::task::JoinHandle;

use crate::depth::feed::{self, ExportSink, SCAN_TOPIC};
use crate::engine::VisionEngine;

/// The topics for a depth layer (`scan`) and a tracker (`beacon`).
pub fn topics(scan: bool, beacon: bool) -> Vec<&'static str> {
    [(scan, SCAN_TOPIC), (beacon, FOLLOW_BEACON_TOPIC)]
        .into_iter()
        .filter_map(|(on, t)| on.then_some(t))
        .collect()
}

/// Read the export into the depth fit's ranging and the subject estimate
/// until `cancel`; `None` when neither wants anything from it.
pub fn spawn(
    engine: &Arc<VisionEngine>,
    scan: bool,
    beacon: bool,
    cancel: &Shutdown,
) -> Option<JoinHandle<()>> {
    let topics = topics(scan, beacon);
    if topics.is_empty() {
        return None;
    }
    let (ranging, engine) = (engine.range_feed(), engine.clone());
    let sink: ExportSink = Arc::new(move |line, at_ms| {
        if !ranging.apply_event_line(line, at_ms) {
            engine.apply_beacon_line(line, at_ms);
        }
    });
    Some(tokio::spawn(feed::run_export_reader(
        feed::default_events_socket(),
        topics,
        sink,
        cancel.clone(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_consumer_subscribes_only_its_topic() {
        assert_eq!(topics(true, false), [SCAN_TOPIC]);
        assert_eq!(topics(false, true), [FOLLOW_BEACON_TOPIC]);
        assert_eq!(topics(true, true).len(), 2);
        assert!(topics(false, false).is_empty());
    }
}
