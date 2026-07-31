//! Report what this receiver actually decoded back to the transmitting drone.
//!
//! The drone's adaptive bitrate ladder needs a measured loss figure, and a
//! transmitting node structurally cannot produce one for its own downlink: a
//! single radio in monitor mode does not capture its own injected frames. This
//! ground station does decode the stream and does count the losses, so it sends
//! that count up the aux uplink once a second and the drone's ladder steps on a
//! real sample.
//!
//! Best-effort by design. The feedback is an optimisation of the video rate, not
//! a control path: a datagram that does not arrive costs one sample, and the
//! drone's own staleness gate parks the ladder if enough of them go missing. So
//! a send error is counted and dropped, never retried into a backlog that would
//! deliver a stale measurement late — a late loss figure is worse than none,
//! because the ladder would act on a link state that has already moved.

use std::sync::Arc;
use std::time::Duration;

use ados_protocol::aux_mux::{self, AuxChannel};
use ados_protocol::link_feedback::LinkFeedback;
use ados_radio::link_quality::LinkStats;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

/// How often a sample is reported. Matches the drone ladder's own 1 Hz tick, so
/// each tick there has at most one fresh sample to consider and the two sides
/// cannot beat against each other.
pub const FEEDBACK_INTERVAL: Duration = Duration::from_secs(1);

/// Build the wire record from a receive-side snapshot.
///
/// `has_measurement` is the same real-decode gate the link-state derivation
/// uses: an empty timestamp with zero decoded packets is the default sentinel
/// meaning "nothing decoded", NOT "a clean link". Reporting the sentinel as a
/// measurement would tell the drone its link is perfect at the exact moment the
/// ground station went deaf, which is the most damaging possible lie here — it
/// would hold or raise the rate on a link that had just failed.
pub fn feedback_from(stats: &LinkStats, target_slot: u8) -> LinkFeedback {
    let has_measurement = !stats.timestamp.is_empty() && stats.packets_received > 0;
    LinkFeedback {
        loss_percent: if has_measurement {
            stats.loss_percent
        } else {
            0.0
        },
        rssi_dbm: stats.rssi_dbm,
        snr_db: stats.snr_db,
        packets_received: stats.packets_received.clamp(0, i64::from(u32::MAX)) as u32,
        fec_failed: stats.fec_failed.clamp(0, i64::from(u32::MAX)) as u32,
        bitrate_kbps: stats.bitrate_kbps.clamp(0, i64::from(u32::MAX)) as u32,
        has_measurement,
        target_slot,
    }
}

/// 1 Hz feedback emitter. Runs until the task is aborted with its generation.
///
/// `aux_tx_port` is the ground station's aux uplink ingress — the same port the
/// relay-proxy request lane and the GCS uplink already write to, so this adds a
/// channel to a lane that is already spawned rather than a second transmitter.
///
/// `measured_slot` is the slot these stats actually describe. This station runs
/// ONE stats reader, on the primary chain, so today that is the primary slot
/// and every other drone correctly gets no sample. The uplink reaches the whole
/// fleet in one transmission, so without this the record would read as every
/// drone's own link and hand the fleet the primary's loss.
pub async fn run(link: Arc<Mutex<LinkStats>>, aux_tx_port: u16, measured_slot: u8) {
    let sock = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "link_feedback_socket_bind_failed");
            return;
        }
    };
    let target = format!("127.0.0.1:{aux_tx_port}");
    let mut tick = tokio::time::interval(FEEDBACK_INTERVAL);
    let mut send_errors: u64 = 0;
    loop {
        tick.tick().await;
        let stats = link.lock().await.clone();
        let payload = feedback_from(&stats, measured_slot).encode();
        let Some(frame) = aux_mux::encode(AuxChannel::LinkFeedback, &payload) else {
            // Structurally impossible for a fixed 20-byte record, but encoding
            // is fallible at the type level and a silent `unwrap` here would
            // take the whole emitter down on a future field addition.
            tracing::warn!(len = payload.len(), "link_feedback_frame_too_long");
            continue;
        };
        if let Err(e) = sock.send_to(&frame, &target).await {
            send_errors = send_errors.saturating_add(1);
            // Log the first failure and then every 60th, so a persistently down
            // uplink leaves a trail without flooding a 1 Hz loop into the log.
            if send_errors == 1 || send_errors.is_multiple_of(60) {
                tracing::warn!(error = %e, target = %target, send_errors, "link_feedback_send_failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured() -> LinkStats {
        LinkStats {
            packets_received: 485,
            loss_percent: 24.29,
            rssi_dbm: -36.0,
            snr_db: 12.4,
            fec_failed: 25,
            bitrate_kbps: 2242,
            timestamp: "2026-07-31T12:00:00Z".to_string(),
            ..LinkStats::default()
        }
    }

    #[test]
    fn a_real_snapshot_reports_its_loss() {
        let fb = feedback_from(&measured(), 1);
        assert!(fb.has_measurement);
        assert!((fb.loss_percent - 24.29).abs() < 0.001);
        assert_eq!(fb.packets_received, 485);
        assert_eq!(fb.fec_failed, 25);
        assert_eq!(fb.bitrate_kbps, 2242);
    }

    #[test]
    fn the_default_sentinel_is_not_reported_as_a_clean_link() {
        // The regression this guards: LinkStats::default() has loss_percent 0.0
        // and packets_received 0. Shipping that as a measurement would tell the
        // drone "0% loss" at the moment this receiver went deaf.
        let fb = feedback_from(&LinkStats::default(), 1);
        assert!(
            !fb.has_measurement,
            "the no-decode sentinel must not claim to be a measurement"
        );
    }

    #[test]
    fn a_timestamped_snapshot_with_no_packets_is_still_not_a_measurement() {
        let stats = LinkStats {
            timestamp: "2026-07-31T12:00:00Z".to_string(),
            packets_received: 0,
            ..LinkStats::default()
        };
        assert!(!feedback_from(&stats, 1).has_measurement);
    }

    #[test]
    fn loss_is_zeroed_when_there_is_no_measurement_so_it_cannot_be_read_raw() {
        // A consumer that ignores the flag must not find a stale loss figure
        // sitting in the field.
        let stats = LinkStats {
            loss_percent: 99.0,
            packets_received: 0,
            ..LinkStats::default()
        };
        let fb = feedback_from(&stats, 1);
        assert!(!fb.has_measurement);
        assert_eq!(fb.loss_percent, 0.0);
    }

    #[test]
    fn counters_beyond_u32_saturate_rather_than_wrapping() {
        let stats = LinkStats {
            packets_received: i64::MAX,
            fec_failed: i64::MAX,
            bitrate_kbps: i64::MAX,
            timestamp: "t".to_string(),
            ..LinkStats::default()
        };
        let fb = feedback_from(&stats, 1);
        assert_eq!(fb.packets_received, u32::MAX);
        assert_eq!(fb.fec_failed, u32::MAX);
        assert_eq!(fb.bitrate_kbps, u32::MAX);
    }

    #[test]
    fn the_emitted_frame_round_trips_through_the_aux_mux() {
        let payload = feedback_from(&measured(), 1).encode();
        let frame = aux_mux::encode(AuxChannel::LinkFeedback, &payload).expect("frame");
        let (channel, inner) = aux_mux::decode(&frame).expect("decode");
        assert_eq!(channel, AuxChannel::LinkFeedback);
        let back = LinkFeedback::decode(inner).expect("record");
        assert!((back.loss_percent - 24.29).abs() < 0.01);
    }
}
