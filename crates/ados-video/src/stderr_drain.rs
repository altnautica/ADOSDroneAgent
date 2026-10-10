//! Plain rate-limited stderr drain for the encoder / cloud-push / SEI-tap
//! subprocesses.
//!
//! These are the children that are NOT the wfb tap: they have no `-progress`
//! token to count (the encoder is the source; its liveness is asserted through
//! the mediamtx inbound-byte counter, not its own stderr). Attaching the
//! wfb-tee `ProgressTracker` to the encoder would conflate two independent
//! liveness signals — so these get this plain drain instead.
//!
//! The drain still matters for two reasons: an undrained stderr pipe fills at
//! 64 KB and blocks the child's next write (freezing it while it still looks
//! alive), and a child hammering a dead device (ffmpeg against a `/dev/video`
//! node that no longer opens) can emit tens of lines a second. We drain every
//! line so the pipe never deadlocks, but cap the logged output to a few lines
//! per window with a single suppressed-count summary — the same shape as the
//! Python `_drain_stderr`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Lower-cased fragments of ffmpeg / GStreamer lines that name a failure of
/// the ENCODER itself (codec init, frame submission, a hardware encoder
/// element), as opposed to the source (a camera or network input that
/// delivers nothing) or the publish destination.
const ENCODER_ERROR_MARKERS: &[&str] = &[
    "error initializing output stream",
    "error while opening encoder",
    "could not open encoder",
    "error submitting video frame",
    "error encoding",
    "error while encoding",
];
/// Hardware encoder names whose own error lines are encoder-side failures.
const HW_ENCODER_NAMES: &[&str] = &["v4l2m2m", "mpph264enc", "omxh264", "v4l2h264enc"];

/// Whether a stderr line reports a failure of the encoder itself. Only such a
/// failure counts toward abandoning the hardware encoder for software.
pub fn is_encoder_side_error(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    ENCODER_ERROR_MARKERS.iter().any(|m| l.contains(m))
        || (HW_ENCODER_NAMES.iter().any(|n| l.contains(n))
            && (l.contains("error") || l.contains("fail")))
}

/// The longest stderr line kept, in bytes; the rest of a longer line is
/// discarded up to its terminator.
pub const MAX_LINE_BYTES: usize = 4096;

/// A line reader for child stderr that can neither grow without bound nor
/// wait forever on a newline.
///
/// `BufReader::lines` splits on `\n` only and buffers the whole line first.
/// ffmpeg's periodic stats report (the default `-stats` output of the 5.x CLI)
/// ends every update with `\r` and never with `\n`, so a healthy encoder's
/// stderr is one endless "line": the drain's buffer grew for the life of the
/// stream and the first real warning then logged the whole accumulated blob.
/// This splits on either terminator and caps each line at [`MAX_LINE_BYTES`].
pub struct BoundedLines<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
    eof: bool,
}

impl<R: AsyncRead + Unpin> BoundedLines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            line: Vec::new(),
            eof: false,
        }
    }

    /// The next `\n`- or `\r`-terminated line (possibly empty), truncated to
    /// [`MAX_LINE_BYTES`]. `None` once the stream has ended or failed and any
    /// unterminated tail has been returned.
    pub async fn next_line(&mut self) -> Option<String> {
        loop {
            if self.eof {
                if self.line.is_empty() {
                    return None;
                }
                return Some(self.take_line());
            }
            let buf = match self.reader.fill_buf().await {
                Ok(buf) if !buf.is_empty() => buf,
                _ => {
                    self.eof = true;
                    continue;
                }
            };
            match buf.iter().position(|b| *b == b'\n' || *b == b'\r') {
                Some(end) => {
                    push_bounded(&mut self.line, &buf[..end]);
                    self.reader.consume(end + 1);
                    return Some(self.take_line());
                }
                None => {
                    let n = buf.len();
                    push_bounded(&mut self.line, buf);
                    self.reader.consume(n);
                }
            }
        }
    }

    fn take_line(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        text
    }
}

fn push_bounded(line: &mut Vec<u8>, bytes: &[u8]) {
    let room = MAX_LINE_BYTES.saturating_sub(line.len());
    line.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

/// At most this many real-diagnostic lines per [`DRAIN_WINDOW`] reach the log.
const DRAIN_MAX_LINES_PER_WINDOW: u32 = 5;
/// Rolling window for the rate limit.
const DRAIN_WINDOW: Duration = Duration::from_secs(10);

/// What one drain reported, so a caller (or a test) can assert on it without
/// having to capture a tracing subscriber.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainStats {
    /// Lines forwarded to the log.
    pub logged: u32,
    /// Lines dropped by the rate limit and reported only as a count.
    pub suppressed: u32,
}

/// Drain `stderr` to completion, logging real diagnostics at `warn` up to the
/// per-window rate limit and summarising the rest. `label` identifies the child
/// in the log lines. Runs until the stream closes (the child exited or the
/// handle was dropped).
pub async fn drain_plain<R: AsyncRead + Unpin>(stderr: R, label: &'static str) -> DrainStats {
    drain_classified(stderr, label, None).await
}

/// [`drain_plain`] for the primary encoder: additionally counts every
/// encoder-side error line ([`is_encoder_side_error`]) into `encoder_errors`,
/// whether or not the rate limit let it reach the log.
pub async fn drain_encoder<R: AsyncRead + Unpin>(
    stderr: R,
    label: &'static str,
    encoder_errors: std::sync::Arc<AtomicU32>,
) -> DrainStats {
    drain_classified(stderr, label, Some(&*encoder_errors)).await
}

async fn drain_classified<R: AsyncRead + Unpin>(
    stderr: R,
    label: &'static str,
    encoder_errors: Option<&AtomicU32>,
) -> DrainStats {
    let mut lines = BoundedLines::new(stderr);
    let mut window_start = Instant::now();
    let mut logged: u32 = 0;
    let mut total_logged: u32 = 0;
    let mut suppressed: u32 = 0;
    let mut last_suppressed_line = String::new();

    while let Some(raw) = lines.next_line().await {
        let text = raw.trim_end();
        if text.is_empty() {
            continue;
        }
        if let Some(counter) = encoder_errors {
            if is_encoder_side_error(text) {
                counter.fetch_add(1, Ordering::AcqRel);
            }
        }
        let now = Instant::now();
        if now.duration_since(window_start) >= DRAIN_WINDOW {
            if suppressed > 0 {
                tracing::warn!(
                    label,
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
            tracing::warn!(label, line = %text, "subprocess_stderr");
            logged += 1;
            total_logged += 1;
        } else {
            suppressed += 1;
            last_suppressed_line = text.to_string();
        }
    }

    // Flush on stream close, not only when the next window opens.
    //
    // The summary used to be emitted exclusively at a window boundary, so a
    // child that died INSIDE its first window took its real error with it: the
    // stream closed, the loop ended, and everything past the rate limit was
    // simply never reported. That is the worst case to lose, because a
    // subprocess that fails immediately is failing at startup — the encoder
    // fault that needed manual reproduction to find had logged nothing but a
    // banner for exactly this reason.
    if suppressed > 0 {
        tracing::warn!(
            label,
            suppressed,
            window_s = window_start.elapsed().as_secs_f64(),
            last_line = %last_suppressed_line,
            "subprocess_stderr_suppressed"
        );
    }
    DrainStats {
        logged: total_logged,
        suppressed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ManagedProcess;

    /// A child that dies inside its FIRST rate-limit window must still report
    /// what it suppressed.
    ///
    /// The summary used to be emitted only when the NEXT window opened, so a
    /// fast-failing subprocess took its real error with it: the stream closed,
    /// the loop ended, and everything past the rate limit was never reported.
    /// That is the worst case to lose — a child that fails immediately is
    /// failing at startup, which is exactly when its output matters most. An
    /// encoder fault that needed manual reproduction to find had logged nothing
    /// but a banner for this reason.
    #[tokio::test]
    async fn a_child_dying_in_its_first_window_still_reports_what_it_suppressed() {
        let over = DRAIN_MAX_LINES_PER_WINDOW + 12;
        let mut child = ManagedProcess::spawn(
            "drain-test",
            "sh",
            &[
                "-c".to_string(),
                format!(
                    "i=0; while [ $i -lt {over} ]; do echo boom$i 1>&2; i=$((i+1)); done; exit 1"
                ),
            ],
        )
        .expect("spawn the fast-failing child");
        let stderr = child.take_stderr().expect("stderr is piped");

        let stats = drain_plain(stderr, "test").await;

        assert_eq!(
            stats.logged, DRAIN_MAX_LINES_PER_WINDOW,
            "the rate limit still holds"
        );
        assert!(
            stats.suppressed > 0,
            "the child outran the rate limit and died in the same window, so the \
             overflow must be accounted for rather than silently lost"
        );
        assert_eq!(stats.logged + stats.suppressed, over);
    }

    #[test]
    fn encoder_side_errors_are_told_from_source_errors() {
        for line in [
            "Error initializing output stream 0:0 -- Error while opening encoder",
            "[h264_v4l2m2m @ 0x55] Error opening the encoder device",
            "[h264_v4l2m2m @ 0x55] Failed to set format",
            "Error submitting video frame to the encoder",
            "ERROR: from element /GstPipeline:pipeline0/GstMppH264Enc:mpph264enc0",
        ] {
            assert!(is_encoder_side_error(line), "{line}");
        }
        for line in [
            "[rtsp @ 0x55] method DESCRIBE failed: 404 Not Found",
            "rtsp://192.168.1.50/stream: Connection refused",
            "[video4linux2,v4l2 @ 0x55] Cannot open video device /dev/video0",
            "frame=  120 fps= 30 q=23.0 size= 512kB",
        ] {
            assert!(!is_encoder_side_error(line), "{line}");
        }
    }

    #[tokio::test]
    async fn the_encoder_drain_counts_encoder_side_errors() {
        let stream = b"frame=1\nError while opening encoder for output stream #0:0\n\
[h264_v4l2m2m @ 0x1] Failed to set format\nConnection refused\n";
        let counter = std::sync::Arc::new(AtomicU32::new(0));
        drain_encoder(&stream[..], "test", counter.clone()).await;
        assert_eq!(counter.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn carriage_return_status_lines_are_split_and_every_line_is_bounded() {
        // ffmpeg's stats report: `\r`-terminated updates and never a newline,
        // then a pathological unterminated tail far past the cap.
        let mut stream = Vec::new();
        for i in 0..2000 {
            stream.extend_from_slice(format!("frame={i} fps=30 q=23.0 size=1kB   \r").as_bytes());
        }
        stream.extend(std::iter::repeat_n(b'x', 1 << 20));
        let mut lines = BoundedLines::new(&stream[..]);
        let mut count = 0usize;
        let mut longest = 0usize;
        while let Some(line) = lines.next_line().await {
            count += 1;
            longest = longest.max(line.len());
        }
        assert_eq!(count, 2001, "each \\r-terminated update is its own line");
        assert!(longest <= MAX_LINE_BYTES, "a line is capped, got {longest}");
    }

    #[tokio::test]
    async fn drains_a_flood_to_completion() {
        // A flood of diagnostics must be drained without panicking; the rate
        // limit is internal, the contract is that the drain consumes the whole
        // stream (so the pipe never deadlocks).
        let mut script = String::new();
        for i in 0..30 {
            script.push_str(&format!("echo 'diag line {i}' >&2\n"));
        }
        let mut p = ManagedProcess::spawn("test-drain", "bash", &["-c".into(), script]).unwrap();
        let stderr = p.take_stderr().unwrap();
        drain_plain(stderr, "test").await;
        p.terminate(Duration::from_millis(200)).await;
    }
}
