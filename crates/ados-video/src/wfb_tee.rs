//! WFB radio fan-out tap: the `ffmpeg` subprocess that copies the local
//! mediamtx RTSP stream into RTP datagrams on UDP 5600 for the wfb-ng radio TX
//! process.
//!
//! The wfb-ng TX subprocess (`wfb_tx -u 5600 ...`) listens on UDP
//! 127.0.0.1:5600 for self-contained datagrams to encapsulate as 802.11 frames
//! with FEC. Each UDP datagram going in must survive single-packet loss on its
//! own, so the encoded H.264 is wrapped in RTP (RFC 6184) first: a lost RTP
//! packet costs at most one NAL fragment instead of corrupting the byte stream
//! to the next start code. The receiver wraps with `rtph264depay` against the
//! SDP at [`WFB_VIDEO_SDP`]. `pkt_size` keeps each datagram under the 802.11
//! MTU after wfb-ng overhead.
//!
//! This module is the leaf the orchestrator drives. It provides:
//! - a pure [`wfb_tee_args`] arg-vector builder (parity-critical: a single
//!   wrong ffmpeg flag re-introduces ~1.2 s of mux latency or breaks codec
//!   discovery on the input);
//! - a [`spawn_wfb_tee`] helper that runs that command through
//!   [`crate::process::ManagedProcess`] (the setsid/killpg owner — no second
//!   spawner);
//! - the output-progress watchdog primitives: [`ProgressTracker`] (the shared
//!   "last progress at" clock the stderr drain stamps and the orchestrator's
//!   timer reads) plus [`wfb_tee_progress_is_stale`];
//! - [`drain_wfb_tee_stderr`], the rate-limited stderr drain that stamps the
//!   tracker on each ffmpeg `-progress` token line and suppresses the routine
//!   telemetry block from the log.
//!
//! Process-liveness alone is never proof of work: the ffmpeg PID can be alive
//! and holding UDP 5600 while it pushes nothing. The progress watchdog is the
//! mandatory output-counter check that distinguishes a working tap from a
//! wedged one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::sync::Mutex;

use crate::process::ManagedProcess;

/// Local UDP host the wfb-ng radio reads from on the air side.
pub const WFB_TEE_HOST: &str = "127.0.0.1";
/// Local UDP port the wfb-ng `wfb_tx -u 5600` process listens on.
pub const WFB_TEE_PORT: u16 = 5600;
/// RTP `pkt_size` — keeps each datagram under the 802.11 MTU after wfb-ng FEC.
pub const WFB_TEE_PKT_SIZE: u16 = 1316;
/// RTP payload type for H.264 (RFC 6184 dynamic PT).
pub const WFB_TEE_PAYLOAD_TYPE: u8 = 96;
/// RTP SSRC pinned so the receiver's depay can lock without renegotiation.
pub const WFB_TEE_SSRC: &str = "0xCAFE";

/// Output-progress watchdog window: if the ffmpeg stderr stops emitting
/// `-progress` tokens for this long, the process is a zombie (alive but not
/// pushing UDP packets) and the orchestrator must restart it. 15 s is the
/// practical floor: ffmpeg's RTSP handshake (DESCRIBE / SETUP / PLAY) + first
/// IDR wait can take 5-10 s on a cold-start bench, so a tighter threshold
/// trips false-positive restart cascades during install + reload races.
pub const WFB_TEE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(15);

/// Receiver-side SDP describing the RTP stream wfb-ng delivers.
pub const WFB_VIDEO_SDP: &str = "/etc/ados/wfb/video.sdp";

/// stderr drain rate-limit: at most this many real-diagnostic lines per
/// [`DRAIN_WINDOW`] reach the log; the rest are counted and summarised.
const DRAIN_MAX_LINES_PER_WINDOW: u32 = 5;
/// Rolling window for the stderr-drain rate limit.
const DRAIN_WINDOW: Duration = Duration::from_secs(10);

/// Build the exact `ffmpeg` arg vector for the RTSP → RTP tap.
///
/// Returns the program (`ffmpeg`) followed by its arguments. The flags are
/// byte-for-byte fixed:
/// - `-fflags nobuffer -flags low_delay` keep the input demuxer from buffering;
/// - `-rtsp_transport tcp` pulls the local stream over TCP (no UDP frag of
///   large keyframe NALs on the loopback);
/// - `-c:v copy` re-muxes without re-encoding (the SEI splice already happened
///   upstream of mediamtx);
/// - `-f rtp -payload_type 96 -ssrc 0xCAFE` emit the RTP wrapping;
/// - `-muxdelay 0 -muxpreload 0 -flush_packets 1` strip the RTP muxer's default
///   ~0.7 s mux delay + ~0.5 s preload + output-side packet aggregation;
/// - `-progress pipe:2` forces the periodic status report to stderr as plain
///   `key=value` lines once per second so the watchdog has a token to count
///   (ffmpeg suppresses the status line entirely when stderr is not a tty).
///
/// `-max_delay 0` is deliberately ABSENT here: on the input ffmpeg it breaks
/// codec discovery (same root cause as the mediamtx ingest sidecar).
pub fn wfb_tee_args(rtsp_in: &str, rtp_out: &str) -> Vec<String> {
    vec![
        "-fflags".into(),
        "nobuffer".into(),
        "-flags".into(),
        "low_delay".into(),
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        rtsp_in.to_string(),
        "-c:v".into(),
        "copy".into(),
        "-f".into(),
        "rtp".into(),
        "-payload_type".into(),
        WFB_TEE_PAYLOAD_TYPE.to_string(),
        "-ssrc".into(),
        WFB_TEE_SSRC.to_string(),
        "-muxdelay".into(),
        "0".into(),
        "-muxpreload".into(),
        "0".into(),
        "-flush_packets".into(),
        "1".into(),
        "-progress".into(),
        "pipe:2".into(),
        rtp_out.to_string(),
    ]
}

/// The local mediamtx RTSP source URL the tap reads from.
pub fn local_rtsp_url(rtsp_port: u16) -> String {
    format!("rtsp://localhost:{rtsp_port}/main")
}

/// The RTP destination URL the tap writes to (UDP 5600, sized per the 802.11
/// MTU).
pub fn rtp_destination_url() -> String {
    format!("rtp://{WFB_TEE_HOST}:{WFB_TEE_PORT}?pkt_size={WFB_TEE_PKT_SIZE}")
}

/// Spawn the wfb_tee ffmpeg through [`ManagedProcess`] (setsid/killpg owner).
///
/// The orphan sweep + UDP-5600-contention defence is the caller's job before
/// the (re)spawn: call [`crate::process::kill_orphans`] with a pattern matching
/// the RTP destination so a stale ffmpeg from a prior crashed run cannot fight
/// the fresh one for the socket. This helper composes the source/destination
/// URLs and the arg vector and hands them to the shared spawner.
pub fn spawn_wfb_tee(rtsp_port: u16) -> std::io::Result<ManagedProcess> {
    let rtsp_in = local_rtsp_url(rtsp_port);
    let rtp_out = rtp_destination_url();
    let args = wfb_tee_args(&rtsp_in, &rtp_out);
    ManagedProcess::spawn("wfb_tee", "ffmpeg", &args)
}

/// The pattern [`crate::process::kill_orphans`] sweeps before a (re)spawn: any
/// stray ffmpeg whose command line targets the wfb RTP destination.
pub fn orphan_pattern() -> String {
    format!("rtp://{WFB_TEE_HOST}:{WFB_TEE_PORT}")
}

/// Recognises an ffmpeg `-progress` / status-line token in an stderr line.
///
/// `-progress pipe:2` emits structured `key=value` lines once per second
/// (`frame=`, `fps=`, `out_time_ms=`, `total_size=`, `progress=continue`, ...);
/// the legacy single-line status report uses the same `frame=`/`size=`/`time=`
/// /`bitrate=` tokens. Any of them advancing at ~1 Hz on a healthy bench is the
/// liveness signal. Matching the mere presence of the token (not a strict
/// increase) is sufficient because ffmpeg only re-emits the status while it is
/// actually processing.
pub fn is_progress_token(line: &str) -> bool {
    const TOKENS: &[&str] = &[
        "frame=",
        "size=",
        "time=",
        "bitrate=",
        "out_time_ms=",
        "out_time_us=",
        "out_time=",
        "total_size=",
        "fps=",
        "dup_frames=",
        "drop_frames=",
        "speed=",
        "progress=",
    ];
    // Word-boundary semantics of the Python `\b(?:...)=` alternation: the token
    // key must start at the line start or follow a non-word character so a
    // substring like "reframe=" does not match "frame=".
    let bytes = line.as_bytes();
    for tok in TOKENS {
        let mut from = 0;
        while let Some(rel) = line[from..].find(tok) {
            let at = from + rel;
            let prev_ok =
                at == 0 || !bytes[at - 1].is_ascii_alphanumeric() && bytes[at - 1] != b'_';
            if prev_ok {
                return true;
            }
            from = at + 1;
        }
    }
    false
}

/// True when a line is the routine bare `key=value` telemetry the `-progress`
/// block emits (a lowercase key, then `=`). Used to suppress that block from
/// the log — it is parsed for the liveness stamp but is pure noise at
/// ~12 lines/s. Real ffmpeg diagnostics start with `[component @ addr]`, a
/// capital word, or a path, so they do not match this lowercase shape.
pub fn is_progress_line(line: &str) -> bool {
    let mut chars = line.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    // The key is `[a-z][a-z0-9_]*` and must be immediately followed by `=`.
    for c in line.chars() {
        if c == '=' {
            return true;
        }
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return false;
        }
    }
    false
}

/// Parse the `frame=N` counter from a progress line, if present (observability;
/// does not advance under `-c:v copy` but harmless).
pub fn parse_frame_count(line: &str) -> Option<u64> {
    let idx = line.find("frame=")?;
    // Reject a substring match preceded by a word char (mirror the `\b`).
    if idx > 0 {
        let prev = line.as_bytes()[idx - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    let rest = &line[idx + "frame=".len()..];
    // ffmpeg pads the value with spaces (`frame=  42`); skip them, then read
    // the run of digits.
    let digits: String = rest
        .trim_start_matches(' ')
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// The "last forward-progress observed" clock the stderr drain stamps and the
/// orchestrator's watchdog reads. Cloneable handle over a shared [`Instant`];
/// the orchestrator owns the timer that compares it against
/// [`WFB_TEE_PROGRESS_TIMEOUT`] via [`wfb_tee_progress_is_stale`].
#[derive(Clone)]
pub struct ProgressTracker {
    last_progress_at: Arc<Mutex<Instant>>,
    last_frame_count: Arc<Mutex<i64>>,
}

impl ProgressTracker {
    /// Fresh tracker stamped to `now`, so a just-spawned tap gets the full
    /// [`WFB_TEE_PROGRESS_TIMEOUT`] window before the watchdog can trip.
    pub fn new() -> Self {
        Self {
            last_progress_at: Arc::new(Mutex::new(Instant::now())),
            last_frame_count: Arc::new(Mutex::new(-1)),
        }
    }

    /// Stamp the clock to `at` (called on every observed progress token).
    pub async fn stamp(&self, at: Instant) {
        *self.last_progress_at.lock().await = at;
    }

    /// The most recent progress instant.
    pub async fn last_progress_at(&self) -> Instant {
        *self.last_progress_at.lock().await
    }

    /// Record a frame number if it advances the high-water mark.
    pub async fn observe_frame(&self, frame: u64) {
        let mut guard = self.last_frame_count.lock().await;
        if (frame as i64) > *guard {
            *guard = frame as i64;
        }
    }

    /// The highest frame number seen, or `-1` if none yet.
    pub async fn last_frame_count(&self) -> i64 {
        *self.last_frame_count.lock().await
    }
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// The Rule-37 "alive but wedged" check: true once the tap has gone
/// [`WFB_TEE_PROGRESS_TIMEOUT`] without a fresh progress token.
pub fn wfb_tee_progress_is_stale(last_progress_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_progress_at) >= WFB_TEE_PROGRESS_TIMEOUT
}

/// Drain the wfb_tee ffmpeg stderr, stamping `tracker` on every progress token
/// and rate-limiting the real-diagnostic log to [`DRAIN_MAX_LINES_PER_WINDOW`]
/// per [`DRAIN_WINDOW`].
///
/// Runs until the stderr stream closes (the process exited or the handle was
/// dropped). The orchestrator spawns this as a task alongside the process and
/// reads `tracker` from its watchdog. The routine `-progress` telemetry block
/// is parsed for the liveness stamp but never logged (it is ~12 lines/s of
/// noise); only real ffmpeg diagnostics reach the warn log, and only up to the
/// rate limit, with a suppressed-count summary at the end of each window.
pub async fn drain_wfb_tee_stderr(stderr: ChildStderr, tracker: ProgressTracker) {
    let mut lines = BufReader::new(stderr).lines();
    let mut window_start = Instant::now();
    let mut logged: u32 = 0;
    let mut suppressed: u32 = 0;
    let mut last_suppressed_line = String::new();

    while let Ok(Some(raw)) = lines.next_line().await {
        let text = raw.trim_end();
        if text.is_empty() {
            continue;
        }

        if is_progress_token(text) {
            tracker.stamp(Instant::now()).await;
            if let Some(frame) = parse_frame_count(text) {
                tracker.observe_frame(frame).await;
            }
        }

        // Suppress the routine per-second telemetry block (parsed above for the
        // liveness stamp); only real diagnostics flow past here.
        if is_progress_token(text) || is_progress_line(text) {
            continue;
        }

        let now = Instant::now();
        if now.duration_since(window_start) >= DRAIN_WINDOW {
            if suppressed > 0 {
                tracing::warn!(
                    label = "wfb_tee",
                    suppressed,
                    window_s = now.duration_since(window_start).as_secs_f64(),
                    last_line = %last_suppressed_line,
                    "subprocess_stderr_suppressed"
                );
            }
            window_start = now;
            logged = 0;
            suppressed = 0;
            last_suppressed_line.clear();
        }

        if logged < DRAIN_MAX_LINES_PER_WINDOW {
            tracing::warn!(label = "wfb_tee", line = %text, "subprocess_stderr");
            logged += 1;
        } else {
            suppressed += 1;
            last_suppressed_line = text.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- arg-vector parity ---------------------------------------------

    #[test]
    fn wfb_tee_args_byte_parity() {
        let rtsp_in = local_rtsp_url(8554);
        let rtp_out = rtp_destination_url();
        assert_eq!(rtsp_in, "rtsp://localhost:8554/main");
        assert_eq!(rtp_out, "rtp://127.0.0.1:5600?pkt_size=1316");

        let got = wfb_tee_args(&rtsp_in, &rtp_out);
        // Hand-written expected vector, each token verified against
        // pipeline/wfb_tee.py:206-220 + pipeline/constants.py. `-max_delay 0`
        // is intentionally absent (it breaks input codec discovery).
        let expected: Vec<String> = [
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-rtsp_transport",
            "tcp",
            "-i",
            "rtsp://localhost:8554/main",
            "-c:v",
            "copy",
            "-f",
            "rtp",
            "-payload_type",
            "96",
            "-ssrc",
            "0xCAFE",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-flush_packets",
            "1",
            "-progress",
            "pipe:2",
            "rtp://127.0.0.1:5600?pkt_size=1316",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn no_max_delay_flag_present() {
        let got = wfb_tee_args("rtsp://localhost:8554/main", "rtp://127.0.0.1:5600");
        assert!(
            !got.iter().any(|t| t == "-max_delay"),
            "-max_delay is forbidden on the wfb_tee input ffmpeg"
        );
    }

    #[test]
    fn constants_match_python() {
        assert_eq!(WFB_TEE_HOST, "127.0.0.1");
        assert_eq!(WFB_TEE_PORT, 5600);
        assert_eq!(WFB_TEE_PKT_SIZE, 1316);
        assert_eq!(WFB_TEE_PAYLOAD_TYPE, 96);
        assert_eq!(WFB_TEE_SSRC, "0xCAFE");
        assert_eq!(WFB_TEE_PROGRESS_TIMEOUT, Duration::from_secs(15));
        assert_eq!(WFB_VIDEO_SDP, "/etc/ados/wfb/video.sdp");
        assert_eq!(orphan_pattern(), "rtp://127.0.0.1:5600");
    }

    // --- progress detection --------------------------------------------

    #[test]
    fn progress_token_recognition() {
        // The -progress pipe:2 key=value lines.
        assert!(is_progress_token("frame=42"));
        assert!(is_progress_token("out_time_ms=1234567"));
        assert!(is_progress_token("total_size=98765"));
        assert!(is_progress_token("progress=continue"));
        assert!(is_progress_token("bitrate= 4096.0kbits/s"));
        // The legacy single-line status report.
        assert!(is_progress_token(
            "frame=  42 fps= 30 q=-1.0 size=  1024kB time=00:00:01.40 bitrate=..."
        ));
        // A real diagnostic must NOT be treated as progress.
        assert!(!is_progress_token(
            "[rtsp @ 0x55] method DESCRIBE failed: 404 Not Found"
        ));
        // Word-boundary: a substring must not false-match.
        assert!(!is_progress_token("keyframe=skip"));
        assert!(!is_progress_token("xbitrate=1"));
    }

    #[test]
    fn progress_line_suppression_shape() {
        // Bare lowercase key=value → suppressed telemetry.
        assert!(is_progress_line("frame=42"));
        assert!(is_progress_line("stream_0_0_q=-1.0"));
        assert!(is_progress_line("progress=continue"));
        // Real diagnostics start with a bracket / capital / path → not matched.
        assert!(!is_progress_line("[rtsp @ 0x55] error"));
        assert!(!is_progress_line("Error opening input"));
        assert!(!is_progress_line("/dev/video0: No such file"));
        assert!(!is_progress_line(""));
    }

    #[test]
    fn frame_count_parsing() {
        assert_eq!(parse_frame_count("frame=42"), Some(42));
        assert_eq!(parse_frame_count("frame=  108 fps=30"), Some(108));
        assert_eq!(parse_frame_count("size=1024kB"), None);
        assert_eq!(parse_frame_count("keyframe=12"), None);
    }

    // --- stale watchdog boundary ---------------------------------------

    #[test]
    fn progress_stale_boundary_at_15s() {
        let base = Instant::now();
        // Just under 15 s → not stale.
        let almost = base + Duration::from_millis(14_999);
        assert!(!wfb_tee_progress_is_stale(base, almost));
        // Exactly 15 s → stale (>= threshold).
        let at = base + Duration::from_secs(15);
        assert!(wfb_tee_progress_is_stale(base, at));
        // Past 15 s → stale.
        let past = base + Duration::from_secs(30);
        assert!(wfb_tee_progress_is_stale(base, past));
        // now == last_progress (just stamped) → not stale.
        assert!(!wfb_tee_progress_is_stale(base, base));
    }

    // --- tracker --------------------------------------------------------

    #[tokio::test]
    async fn tracker_stamp_and_frame_advance() {
        let t = ProgressTracker::new();
        assert_eq!(t.last_frame_count().await, -1);
        let before = t.last_progress_at().await;
        let later = before + Duration::from_secs(5);
        t.stamp(later).await;
        assert_eq!(t.last_progress_at().await, later);

        t.observe_frame(10).await;
        assert_eq!(t.last_frame_count().await, 10);
        // Non-advancing frame does not move the high-water mark.
        t.observe_frame(7).await;
        assert_eq!(t.last_frame_count().await, 10);
        t.observe_frame(11).await;
        assert_eq!(t.last_frame_count().await, 11);
    }

    // --- stderr drain over a real pipe ---------------------------------

    /// Spawn `bash -c "<emit>"` through ManagedProcess and drain its stderr so
    /// the test exercises the real `ChildStderr` line stream the orchestrator
    /// hands in.
    async fn drain_emitted(emit_script: &str) -> ProgressTracker {
        let mut p = ManagedProcess::spawn(
            "test-wfb-tee",
            "bash",
            &["-c".into(), emit_script.to_string()],
        )
        .unwrap();
        let stderr = p.take_stderr().unwrap();
        let tracker = ProgressTracker::new();
        drain_wfb_tee_stderr(stderr, tracker.clone()).await;
        p.terminate(Duration::from_millis(200)).await;
        tracker
    }

    #[tokio::test]
    async fn drain_stamps_progress_and_advances_frame() {
        // Emit a real diagnostic, then a progress block, to stderr (>&2).
        let script = r#"
            echo "frame=5" >&2
            echo "out_time_ms=200000" >&2
            echo "progress=continue" >&2
        "#;
        let tracker = drain_emitted(script).await;
        // A progress token advanced the frame counter.
        assert_eq!(tracker.last_frame_count().await, 5);
        // The stamp advanced past the tracker's construction instant.
        // (Can't assert an exact value; assert it is not stale immediately.)
        let last = tracker.last_progress_at().await;
        assert!(!wfb_tee_progress_is_stale(last, Instant::now()));
    }

    #[tokio::test]
    async fn drain_rate_limits_diagnostics() {
        // Emit 20 real diagnostic lines in one window; the drain logs at most
        // 5 and counts the rest. We assert it consumes them without panicking
        // and stamps no progress (none are progress tokens). The rate-limit
        // accounting is internal; the contract under test is that a flood is
        // drained to completion and progress is NOT stamped on diagnostics.
        let mut script = String::new();
        for i in 0..20 {
            script.push_str(&format!("echo '[rtsp @ 0x{i:x}] retrying' >&2\n"));
        }
        let mut p = ManagedProcess::spawn("test-wfb-tee", "bash", &["-c".into(), script]).unwrap();
        let stderr = p.take_stderr().unwrap();
        let tracker = ProgressTracker::new();
        let before = tracker.last_progress_at().await;
        drain_wfb_tee_stderr(stderr, tracker.clone()).await;
        p.terminate(Duration::from_millis(200)).await;
        // No progress token among the diagnostics → stamp unchanged.
        assert_eq!(tracker.last_progress_at().await, before);
    }

    #[test]
    fn drain_constants_match_python() {
        // 5 lines / 10 s rate limit (pipeline/wfb_tee.py:79-80).
        assert_eq!(DRAIN_MAX_LINES_PER_WINDOW, 5);
        assert_eq!(DRAIN_WINDOW, Duration::from_secs(10));
    }
}
