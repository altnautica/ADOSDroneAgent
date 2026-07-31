//! The receive-plane run loops: the stdout stats reader and the stdout-silence
//! zombie watchdog.
//!
//! `stats_reader_loop` reads `wfb_rx` stdout line-by-line, feeds the link
//! monitor, updates the shared counter + LinkStats + the stdout-liveness stamp
//! on every parsed line, and publishes the ground sidecar + the store event +
//! the metric samples whenever the snapshot they describe has moved (or the
//! re-assert interval is up, so an unchanged link can never age into reading as
//! stale). `zombie_watchdog` terminates the data RX when its per-second stats
//! stream stalls while the process is alive (process-liveness alone is never
//! proof of work).
//!
//! The in-process seams the watchdog and the acquirer read — the valid-decode
//! counter, the shared `LinkStats`, the stdout stamp — are deliberately NOT
//! gated: only the outward publications are, so nothing that decides whether to
//! kill or retune the radio ever sees a coarser picture than it did before.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use ados_protocol::logd::emitter::IngestEmitter;
use ados_radio::config::WfbConfig;
use ados_radio::link_quality::{LinkQualityMonitor, LinkStats};

use crate::watchdog::{Clock, SharedRxHealth, RX_HEALTH_SILENCE_THRESHOLD_S};

use super::args::{RX_HEALTH_POLL_INTERVAL_S, STATE_ACTIVE, STATE_SEARCHING};
use super::seams::{live_channel, DataRxHandle, SharedValidCounter};
use super::stats::{
    build_gs_stats, json_object_to_fields, GsAdapterInfo, GsChannelTruth, GsRegSnapshot,
};

/// How often the live interface channel is re-read from `iw`, in seconds.
///
/// The netdev's channel moves only when the acquirer retunes it, which is not a
/// per-second event, so re-reading it on every stats line spent a forked process
/// each second to confirm the value already held. The reading stays a real one
/// and is at most this old; a failed read has always been allowed to leave the
/// last-known value standing, so a bounded age was already the contract.
const LIVE_CHANNEL_POLL_INTERVAL_S: f64 = 5.0;

/// How long an UNCHANGED snapshot may go unpublished, in seconds.
///
/// Suppressing a repeat outright is not an option: consumers age the snapshot
/// from its file mtime and its stored event timestamp, and every one of them
/// calls it stale somewhere between ten and thirty seconds. A ground station
/// that is genuinely searching would then read as stale rather than searching,
/// which is a worse answer than the one the repeats were paying for. Re-asserting
/// well inside the tightest of those ceilings keeps every surface truthful while
/// still cutting the idle publish rate five-fold.
const UNCHANGED_REASSERT_INTERVAL_S: f64 = 5.0;

/// Paces the live-channel re-read so a per-second stats stream does not fork
/// `iw` once per second.
struct ChannelPoll {
    last_at: f64,
}

impl ChannelPoll {
    fn new() -> Self {
        // Negative infinity so the very first stats line reads the channel,
        // rather than starting inside a window it never entered.
        Self {
            last_at: f64::NEG_INFINITY,
        }
    }

    /// True when the channel should be re-read at monotonic time `now`.
    ///
    /// Stamped on the attempt, not on success, so a driver that has stopped
    /// answering `iw` is not re-probed every single second.
    fn due(&mut self, now: f64) -> bool {
        if now - self.last_at < LIVE_CHANNEL_POLL_INTERVAL_S {
            return false;
        }
        self.last_at = now;
        true
    }
}

/// Snapshot keys that advance with the clock alone rather than with a new
/// observation of the link: the snapshot's own stamp, and the seconds-since-last
/// valid decode.
///
/// Two snapshots differing only in these describe the same link. They are
/// excluded from the comparison, never from what is written — a published
/// snapshot always carries both freshly, so the body a reader sees is internally
/// consistent: its silence count is as of its own timestamp, not of now.
const CLOCK_ONLY_KEYS: [&str; 2] = ["timestamp", "rx_silent_seconds"];

/// True when two ground-status snapshots report the same link, ignoring the
/// keys that only track the passage of time.
fn snapshots_agree(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else {
        return a == b;
    };
    let observed = |m: &serde_json::Map<String, serde_json::Value>| {
        m.keys()
            .filter(|k| !CLOCK_ONLY_KEYS.contains(&k.as_str()))
            .count()
    };
    // Counting both sides catches a key that appeared or vanished, which the
    // one-directional key walk below would read as agreement.
    observed(a) == observed(b)
        && a.iter()
            .filter(|(k, _)| !CLOCK_ONLY_KEYS.contains(&k.as_str()))
            .all(|(k, v)| b.get(k) == Some(v))
}

/// Decides whether a freshly built ground-status snapshot is worth publishing.
///
/// A receive plane with nothing to report still emits a stats line every second,
/// and each one used to rewrite the sidecar and ship an event plus two metric
/// samples describing a link that had not moved. This admits a snapshot when it
/// differs from the last published one, and otherwise at the re-assert interval
/// so no consumer can age it into staleness.
struct PublishGate {
    last: Option<serde_json::Value>,
    last_at: f64,
}

impl PublishGate {
    fn new() -> Self {
        Self {
            last: None,
            last_at: 0.0,
        }
    }

    /// True when `payload` must be published at monotonic time `now`.
    fn admit(&mut self, payload: &serde_json::Value, now: f64) -> bool {
        let unchanged = self
            .last
            .as_ref()
            .map(|prev| snapshots_agree(prev, payload))
            .unwrap_or(false);
        if unchanged && now - self.last_at < UNCHANGED_REASSERT_INTERVAL_S {
            return false;
        }
        self.last = Some(payload.clone());
        self.last_at = now;
        true
    }
}

/// Read `wfb_rx` stdout line-by-line, feed the link monitor, update the shared
/// counter + LinkStats + the stdout-liveness stamp, and write the ground
/// `wfb-stats.json` sidecar when the snapshot moves. Ends on EOF (process death)
/// or task abort.
#[allow(clippy::too_many_arguments)]
pub async fn stats_reader_loop(
    stdout: tokio::process::ChildStdout,
    counter: SharedValidCounter,
    link: Arc<Mutex<LinkStats>>,
    last_stdout_at: Arc<Mutex<f64>>,
    clock: Arc<dyn Clock>,
    interface: String,
    channel: u8,
    rendezvous: u8,
    reg: GsRegSnapshot,
    cfg: WfbConfig,
    adapter: GsAdapterInfo,
    health: Option<SharedRxHealth>,
    zombie_kills: Arc<AtomicU32>,
    ingest: Option<IngestEmitter>,
    fanout: crate::fanout::FanoutCounters,
    aux: crate::aux_consumer::AuxCounters,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut mon = LinkQualityMonitor::new();
    // Last successfully-read live channel; seeded to the operating channel so a
    // momentary `iw info` failure keeps reporting the last-known live value.
    let mut last_live_channel = channel;
    let mut channel_poll = ChannelPoll::new();
    let mut gate = PublishGate::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let now_mono = clock.monotonic();
        *last_stdout_at.lock().await = now_mono;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let now_iso = now_iso();
        if let Some(snap) = mon.feed_line(line, &now_iso) {
            // Per-interval valid-decode count feeds the cumulative counter the
            // watchdog/acquirer poll.
            counter.add(snap.packets_received);
            let valid_pps = snap.packets_received as f64; // stats interval = 1 s
            let video_bps = snap.bitrate_kbps as f64 * 1000.0 / 8.0;
            // Lock-state surface: decoding valid video on the current channel ==
            // locked even when no sweep ran. This is the GS received-side proof.
            let (channel_locked, acquire_state) = if snap.packets_received > 0 {
                (true, "locked")
            } else {
                (false, "searching")
            };
            // Top-level lifecycle: "active" ONLY when the RX is actually decoding
            // data. wfb_rx emits a PKT line every second even when it is hearing
            // nothing, so producing stats lines is NOT proof of a working link —
            // hardcoding "active" here made a stone-deaf ground station read as
            // connected. When not decoding, report "searching"; the finer CAUSE
            // (deaf / mis_keyed / jammed) is carried in snap.link_diag.
            let state = if snap.packets_received > 0 {
                STATE_ACTIVE
            } else {
                STATE_SEARCHING
            };
            // Pull the live receive-health counters the watchdogs produce so the
            // sidecar carries real values rather than the previous hardcoded
            // zeros. Absent in tests, where the kills/silence default to zero.
            let (reacquire_kills, rx_silent_seconds) = match &health {
                Some(h) => (h.reacquire_kills(), h.silent_seconds().await),
                None => (0, None),
            };
            let rx_zombie_kills = zombie_kills.load(Ordering::SeqCst);
            *link.lock().await = snap.clone();
            // Truthful channel: read the LIVE interface channel (the acquirer
            // sweep can land it away from the configured/operating channel), with
            // the last-known value held through a transient read failure.
            if channel_poll.due(now_mono) {
                if let Some(live) = live_channel(&interface).await {
                    last_live_channel = live;
                }
            }
            let channels = GsChannelTruth {
                actual: last_live_channel,
                rendezvous,
                operating: channel,
            };
            let mut payload = build_gs_stats(
                &snap,
                &interface,
                &adapter,
                channels,
                &reg,
                &cfg,
                state,
                acquire_state,
                channel_locked,
                valid_pps,
                reacquire_kills,
                rx_zombie_kills,
                rx_silent_seconds,
                video_bps,
            );
            // Fold the cumulative fan-out totals onto the sidecar so the
            // video-pipeline harness can attribute the fan-out hop (decoded
            // packets climbing but `fanout_forwarded` flat isolates the fault to
            // the fan-out; both climbing but the ingest byte-rate flat isolates
            // it to the mediamtx-gs ingest ffmpeg).
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("fanout_forwarded".to_string(), fanout.forwarded().into());
                obj.insert("fanout_drops".to_string(), fanout.drops().into());
                // The auxiliary application lane's full tally, under one key.
                // The video counters above describe the video decoder; these
                // describe the separate lane carrying the drone's MAVLink, so a
                // healthy video link with a dead aux lane (or the reverse) is
                // legible instead of reading as one undifferentiated "link".
                // Serialisation of a flat counter struct cannot fail; a null
                // would be a lie about the lane, so the entry is skipped rather
                // than written as one on the impossible branch.
                if let Ok(v) = serde_json::to_value(aux.snapshot()) {
                    obj.insert("aux_lane".to_string(), v);
                }
            }
            // A link that has not moved is not news. The sidecar, the event and
            // the two metric samples all describe this one snapshot, so they are
            // gated together: publishing some of them would leave the series
            // describing an instant the event does not. The cost is resolution
            // in the metric series while nothing is happening — an idle link
            // contributes a flat sample every few seconds instead of every
            // second — and no fabricated sample is ever emitted to fill it.
            if !gate.admit(&payload, now_mono) {
                continue;
            }
            let path = Path::new(crate::paths::WFB_STATS_JSON);
            if let Err(e) = crate::sidecars::write_json_atomic(path, &payload, 0o644) {
                tracing::debug!(error = %e, "ground_wfb_stats_persist_failed");
            }
            // Ship the same body to the logging store as a single full-snapshot
            // event (the durable read source) plus the loss + bitrate samples
            // that round out the link-history series. Best-effort; an absent
            // logging daemon drops these without disturbing receive.
            if let Some(em) = &ingest {
                use ados_protocol::logd::{Fields, Level, Value};
                em.emit_event(
                    "link.wfb_status",
                    Level::Info,
                    json_object_to_fields(&payload),
                );
                let mut tags = Fields::new();
                tags.insert("direction".to_string(), Value::from("uplink"));
                tags.insert("link".to_string(), Value::from("command"));
                em.emit_metric("link.loss_percent", snap.loss_percent, tags.clone());
                em.emit_metric("link.bitrate_kbps", snap.bitrate_kbps as f64, tags);
            }
        }
    }
}

/// Stdout-silence zombie watchdog: terminate the data RX when its per-second
/// stats stream stalls for `RX_HEALTH_SILENCE_THRESHOLD_S` while the process is
/// alive (process-liveness alone is never proof of work). Returns when it kills
/// once or the process exits.
pub async fn zombie_watchdog(
    rx: Arc<DataRxHandle>,
    last_stdout_at: Arc<Mutex<f64>>,
    clock: Arc<dyn Clock>,
    kills: Arc<AtomicU32>,
) {
    use crate::watchdog::RxProcess;
    // Reset the stamp so we don't carry over silence accumulated while the
    // process spawned; give it a full window to start producing stats.
    *last_stdout_at.lock().await = clock.monotonic();
    while rx.is_running() {
        tokio::time::sleep(std::time::Duration::from_secs_f64(
            RX_HEALTH_POLL_INTERVAL_S,
        ))
        .await;
        let silent_for = clock.monotonic() - *last_stdout_at.lock().await;
        if silent_for >= RX_HEALTH_SILENCE_THRESHOLD_S {
            kills.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(
                silent_seconds = silent_for,
                zombie_kills_total = kills.load(Ordering::SeqCst),
                "ground_wfb_rx_zombie_detected"
            );
            rx.terminate();
            *last_stdout_at.lock().await = clock.monotonic();
            return;
        }
    }
}

/// Current ISO-8601 UTC timestamp for the link-stats `timestamp` field.
fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A deaf receive plane's snapshot: nothing decoded, so every observed field
    /// reads the same second after second while the two clock fields advance.
    fn idle_snapshot(second: u32) -> serde_json::Value {
        json!({
            "state": "searching",
            "link_state": "searching",
            "interface": "wlan1",
            "channel": 149,
            "packets_received": 0,
            "packets_all": 0,
            "bitrate_kbps": 0,
            "loss_percent": 0.0,
            "rssi_dbm": serde_json::Value::Null,
            "rx_silent_seconds": second as f64,
            "timestamp": format!("2026-01-01T00:00:{second:02}Z"),
        })
    }

    #[test]
    fn an_idle_receive_plane_publishes_at_the_re_assert_interval_not_every_second() {
        // A ground station hearing nothing still gets a stats line a second.
        // Only the clock fields move, so the snapshot is not news, and
        // republishing it once a second bought nothing but radio-adjacent I/O.
        let mut gate = PublishGate::new();
        let mut published = 0;
        for second in 0..10u32 {
            if gate.admit(&idle_snapshot(second), second as f64) {
                published += 1;
            }
        }
        // Ten seconds of silence: the first line, then one re-assert per
        // interval. Ungated this is ten.
        assert_eq!(published, 2);
    }

    #[test]
    fn an_unchanged_snapshot_is_still_re_asserted_before_any_consumer_calls_it_stale() {
        // Never republishing would let the sidecar mtime and the stored event
        // age past the ten-second ceiling every consumer treats as stale, so a
        // genuinely-searching node would read as stale. The gap between
        // publishes must stay under that.
        let mut gate = PublishGate::new();
        assert!(gate.admit(&idle_snapshot(0), 0.0));
        let mut last_publish = 0.0;
        let mut widest_gap: f64 = 0.0;
        for tenth in 1..=300u32 {
            let now = f64::from(tenth) / 10.0;
            if gate.admit(&idle_snapshot(tenth), now) {
                widest_gap = widest_gap.max(now - last_publish);
                last_publish = now;
            }
        }
        assert!(
            widest_gap < 10.0,
            "an unchanged link went unpublished for {widest_gap}s, long enough to read as stale"
        );
    }

    #[test]
    fn a_link_that_is_actually_moving_publishes_every_line() {
        // The gate must cost a live link nothing: while packets are arriving no
        // two consecutive snapshots agree, so every line still publishes.
        let mut gate = PublishGate::new();
        let mut published = 0;
        for second in 0..10u32 {
            let mut snap = idle_snapshot(second);
            snap["state"] = json!("active");
            snap["packets_received"] = json!(600 + second);
            snap["bitrate_kbps"] = json!(4000 + second * 7);
            snap["rssi_dbm"] = json!(-51.0);
            if gate.admit(&snap, second as f64) {
                published += 1;
            }
        }
        assert_eq!(published, 10);
    }

    #[test]
    fn a_single_changed_field_publishes_immediately_rather_than_waiting_for_the_interval() {
        // Change detection must not delay news. A link that goes deaf, or a
        // channel that moves under an acquirer sweep, publishes on the line it
        // happens, not at the next re-assert.
        let mut gate = PublishGate::new();
        assert!(gate.admit(&idle_snapshot(0), 0.0));
        assert!(!gate.admit(&idle_snapshot(1), 1.0));
        let mut moved = idle_snapshot(2);
        moved["channel"] = json!(165);
        assert!(gate.admit(&moved, 2.0));
    }

    #[test]
    fn only_the_clock_fields_are_ignored_when_comparing_snapshots() {
        let a = idle_snapshot(0);
        let b = idle_snapshot(9);
        assert!(
            snapshots_agree(&a, &b),
            "the clock fields alone are ignored"
        );

        // A field appearing or vanishing is a change, not agreement.
        let mut grown = idle_snapshot(0);
        grown["decrypt_errors"] = json!(4);
        assert!(!snapshots_agree(&a, &grown));
        let mut shrunk = idle_snapshot(0);
        shrunk.as_object_mut().unwrap().remove("packets_all");
        assert!(!snapshots_agree(&a, &shrunk));

        // A real observation changing is a change even when it is small.
        let mut nudged = idle_snapshot(0);
        nudged["loss_percent"] = json!(0.1);
        assert!(!snapshots_agree(&a, &nudged));
    }

    #[test]
    fn the_live_channel_is_re_read_on_a_bounded_interval_not_every_stats_line() {
        // The netdev channel moves only when the acquirer retunes it, so the
        // per-second `iw` fork re-read a value that was almost always the one
        // already held.
        let mut poll = ChannelPoll::new();
        let reads = (0..30u32).filter(|s| poll.due(f64::from(*s))).count();
        // Thirty stats lines forked `iw` thirty times; now it is once per
        // interval, starting with the first line so the value is never guessed.
        assert_eq!(reads, 6);
        assert!(
            ChannelPoll::new().due(0.0),
            "the first stats line must read the channel, not assume it"
        );
    }
}
