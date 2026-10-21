//! The live log stream: a reader thread over `GET /api/logs/stream`, the
//! server-sent-events line parser, and the de-duplicating log ring the Logs
//! tab draws from.
//!
//! The thread reads one line at a time and checks its stop flag between
//! lines, so dropping the [`LogStream`] ends it at the next line, keep-alive
//! comment, or read error.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use ados_protocol::rest::RestClient;
use serde_json::Value;

/// Log lines kept in memory.
pub const RING_CAP: usize = 5000;

/// One log line in the `{seq, timestamp, level, logger, message}` entry shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The store's row id; `None` when the agent sent none.
    pub seq: Option<u64>,
    /// RFC 3339, or `None` when the time it was logged is unknown.
    pub timestamp: Option<String>,
    pub level: String,
    pub logger: String,
    pub message: String,
    /// Arrival order, assigned by the ring.
    pub n: u64,
}

impl LogEntry {
    /// An entry from its JSON object; `None` for anything that is not one.
    pub fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let text = |k: &str| obj.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let seq = match obj.get("seq") {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        };
        Some(Self {
            seq,
            timestamp: obj
                .get("timestamp")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            level: text("level"),
            logger: text("logger"),
            message: text("message"),
            n: 0,
        })
    }

    /// `HH:MM:SS.mmm` from the timestamp, or a dashed placeholder.
    pub fn time(&self) -> String {
        self.timestamp
            .as_deref()
            .and_then(clock_of)
            .unwrap_or_else(|| "--:--:--.---".to_string())
    }

    /// The level's severity rank (see [`level_rank`]).
    pub fn rank(&self) -> u8 {
        level_rank(&self.level)
    }

    /// The level as the word shown in the level column.
    pub fn level_word(&self) -> &'static str {
        level_word(&self.level)
    }
}

/// The wall-clock part of an RFC 3339 timestamp as `HH:MM:SS.mmm`.
fn clock_of(ts: &str) -> Option<String> {
    let (_, rest) = ts.split_once(['T', ' '])?;
    let hms = rest.get(..8)?;
    if hms.as_bytes().get(2) != Some(&b':') {
        return None;
    }
    let frac: String = rest[8..]
        .strip_prefix('.')
        .map(|f| f.chars().take_while(char::is_ascii_digit).take(3).collect())
        .unwrap_or_default();
    Some(format!("{hms}.{frac:0<3}"))
}

/// Severity rank: TRACE 0, DEBUG 1, INFO 2, WARNING 3, ERROR 4, CRITICAL 5.
/// An empty or unknown level ranks as INFO.
pub fn level_rank(level: &str) -> u8 {
    match level.to_ascii_uppercase().as_str() {
        "TRACE" => 0,
        "DEBUG" => 1,
        "WARN" | "WARNING" => 3,
        "ERROR" => 4,
        "CRITICAL" | "FATAL" => 5,
        _ => 2,
    }
}

/// The display word for a level (`WARN` reads as `WARNING`).
pub fn level_word(level: &str) -> &'static str {
    match level_rank(level) {
        0 => "TRACE",
        1 => "DEBUG",
        3 => "WARNING",
        4 => "ERROR",
        5 => "CRITICAL",
        _ => "INFO",
    }
}

/// What the reader thread reports.
#[derive(Debug)]
pub enum LogMsg {
    /// The stream connected.
    Up,
    Entry(LogEntry),
    /// The stream failed or ended, with the reason.
    Down(String),
}

/// Server-sent-events framing: `data:` lines accumulate until a blank line
/// ends the event; `:` comment lines are keep-alives (the last one is kept as
/// the close reason); other fields are ignored.
#[derive(Debug, Default)]
pub struct SseParser {
    data: Vec<String>,
    /// The text of the last comment line seen.
    pub last_comment: Option<String>,
}

impl SseParser {
    /// Feed one line; returns the event payload when the line ends an event.
    pub fn line(&mut self, line: &str) -> Option<String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if self.data.is_empty() {
                return None;
            }
            let payload = self.data.join("\n");
            self.data.clear();
            return Some(payload);
        }
        if let Some(comment) = line.strip_prefix(':') {
            let comment = comment.trim();
            if !comment.is_empty() {
                self.last_comment = Some(comment.to_string());
            }
            return None;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
        None
    }
}

/// The in-memory log: arrival-ordered, capped at [`RING_CAP`], and
/// de-duplicated by `seq` (the stream replays recent rows on every connect and
/// the poll fallback re-reads the same window).
#[derive(Debug)]
pub struct LogRing {
    entries: VecDeque<LogEntry>,
    seen: HashSet<u64>,
    cap: usize,
    next: u64,
}

impl Default for LogRing {
    fn default() -> Self {
        Self::with_cap(RING_CAP)
    }
}

impl LogRing {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            seen: HashSet::new(),
            cap: cap.max(1),
            next: 0,
        }
    }

    /// Append an entry; `false` when its `seq` is already held.
    pub fn push(&mut self, mut entry: LogEntry) -> bool {
        if let Some(seq) = entry.seq {
            if !self.seen.insert(seq) {
                return false;
            }
        }
        entry.n = self.next;
        self.next += 1;
        self.entries.push_back(entry);
        while self.entries.len() > self.cap {
            if let Some(old) = self.entries.pop_front() {
                if let Some(seq) = old.seq {
                    self.seen.remove(&seq);
                }
            }
        }
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.seen.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = &LogEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The arrival number the next entry gets (entries ever accepted).
    pub fn next_n(&self) -> u64 {
        self.next
    }
}

/// A running stream reader. Dropping it stops the thread.
pub struct LogStream {
    stop: Arc<AtomicBool>,
    rx: Receiver<LogMsg>,
}

impl LogStream {
    /// Open `path` on a dedicated thread.
    pub fn spawn(client: RestClient, path: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let flag = Arc::clone(&stop);
        thread::spawn(move || read_stream(&client, &path, &flag, &tx));
        Self { stop, rx }
    }

    /// Everything the thread sent since the last call.
    pub fn drain(&self) -> Vec<LogMsg> {
        self.rx.try_iter().collect()
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn read_stream(client: &RestClient, path: &str, stop: &AtomicBool, tx: &Sender<LogMsg>) {
    let reader = match client.open_stream(path) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(LogMsg::Down(e.to_string()));
            return;
        }
    };
    if stop.load(Ordering::Relaxed) || tx.send(LogMsg::Up).is_err() {
        return;
    }
    let mut parser = SseParser::default();
    for line in BufReader::new(reader).lines() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                let _ = tx.send(LogMsg::Down(format!("stream error: {e}")));
                return;
            }
        };
        let Some(payload) = parser.line(&line) else { continue };
        let entry = serde_json::from_str::<Value>(&payload)
            .ok()
            .and_then(|v| LogEntry::from_value(&v));
        if let Some(entry) = entry {
            if tx.send(LogMsg::Entry(entry)).is_err() {
                return;
            }
        }
    }
    if !stop.load(Ordering::Relaxed) {
        let why = parser.last_comment.unwrap_or_else(|| "stream closed".to_string());
        let _ = tx.send(LogMsg::Down(why));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, level: &str) -> LogEntry {
        LogEntry::from_value(&json!({
            "seq": seq, "timestamp": "2026-10-21T12:04:31.123456Z",
            "level": level, "logger": "ados.test", "message": format!("m{seq}"),
        }))
        .unwrap()
    }

    #[test]
    fn sse_multi_line_data_joins_until_blank() {
        let mut p = SseParser::default();
        assert_eq!(p.line("data: {\"seq\":1,"), None);
        assert_eq!(p.line("data: \"level\":\"INFO\"}"), None);
        let payload = p.line("").unwrap();
        assert_eq!(payload, "{\"seq\":1,\n\"level\":\"INFO\"}");
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["seq"], 1);
    }

    #[test]
    fn sse_comment_lines_are_ignored() {
        let mut p = SseParser::default();
        assert_eq!(p.line(": keep-alive"), None);
        assert_eq!(p.line(""), None);
        assert_eq!(p.line("data: {\"seq\":2}\r"), None);
        assert_eq!(p.line(": ping"), None);
        assert_eq!(p.line("\r").as_deref(), Some("{\"seq\":2}"));
        assert_eq!(p.last_comment.as_deref(), Some("ping"));
    }

    #[test]
    fn ring_drops_duplicate_seq() {
        let mut r = LogRing::default();
        assert!(r.push(entry(7, "INFO")));
        assert!(!r.push(entry(7, "INFO")));
        assert!(r.push(entry(8, "INFO")));
        assert_eq!(r.len(), 2);
        assert_eq!(r.next_n(), 2);
    }

    #[test]
    fn ring_is_capped_and_forgets_evicted_seqs() {
        let mut r = LogRing::with_cap(3);
        for s in 0..5 {
            r.push(entry(s, "INFO"));
        }
        let seqs: Vec<u64> = r.iter().filter_map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4]);
        assert!(r.push(entry(0, "INFO")), "an evicted seq is no longer a duplicate");
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn default_ring_holds_5000() {
        let mut r = LogRing::default();
        for s in 0..5100 {
            r.push(entry(s, "INFO"));
        }
        assert_eq!(r.len(), RING_CAP);
        assert_eq!(r.iter().next().and_then(|e| e.seq), Some(100));
    }

    #[test]
    fn level_ordering() {
        let order = ["TRACE", "DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"];
        for pair in order.windows(2) {
            assert!(level_rank(pair[0]) < level_rank(pair[1]), "{pair:?}");
        }
        assert_eq!(level_rank("WARN"), level_rank("warning"));
        assert_eq!(level_word("warn"), "WARNING");
        assert_eq!(level_rank(""), level_rank("INFO"));
    }

    #[test]
    fn time_is_clock_with_millis() {
        assert_eq!(entry(1, "INFO").time(), "12:04:31.123");
        let mut e = entry(1, "INFO");
        e.timestamp = Some("2026-10-21T12:04:31+00:00".into());
        assert_eq!(e.time(), "12:04:31.000");
        e.timestamp = None;
        assert_eq!(e.time(), "--:--:--.---");
    }
}
