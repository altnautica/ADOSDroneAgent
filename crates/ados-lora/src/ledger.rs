//! The persisted airtime ledger (charge-then-send).
//!
//! One JSON line per admitted frame, appended and fsync'd before the frame
//! reaches the transport, so a crash mid-transmit over-counts and never
//! under-counts, and a reboot cannot reset the duty budget. A `seen`
//! heartbeat line every minute bounds how much downtime is credited.
//!
//! Ageing on load:
//! - an entry from this boot keeps its monotonic time;
//! - an entry from a previous boot is aged by UTC when both it and the
//!   current wall clock are trusted;
//! - otherwise, when the current clock is trusted and its boot left a trusted
//!   heartbeat, it is placed at that heartbeat (downtime since is credited);
//! - otherwise it is treated as having occurred at service start.
//!
//! A corrupt last line (a torn write) is dropped. Any other corrupt line moves
//! the file aside as `.bad` and every duty rule starts exhausted for one full
//! window.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The clocks a governor decision is taken at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// `CLOCK_MONOTONIC` in ns (this boot).
    pub mono_ns: i64,
    /// Wall clock, ms since the Unix epoch.
    pub utc_ms: i64,
    /// Whether the wall clock is trusted (NTP-synchronised or GNSS-locked).
    pub utc_trusted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Tx,
    Seen,
}

/// One ledger line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub boot_id: String,
    pub t_mono_ns: i64,
    pub t_utc_ms: i64,
    pub utc_trusted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_band: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_mhz: Option<f64>,
    #[serde(default)]
    pub airtime_us: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
}

/// A charged transmission placed on this boot's monotonic timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Spend {
    /// Effective time, ns on this boot's monotonic clock (may be negative for
    /// a spend before this boot).
    pub at_ns: i64,
    pub channel_mhz: f64,
    pub airtime_us: u64,
    entry: Entry,
}

/// What loading found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoadReport {
    pub entries: usize,
    pub torn_tail: bool,
    /// The file was corrupt and moved aside; duty starts exhausted.
    pub reset: bool,
}

pub struct Ledger {
    path: PathBuf,
    file: Option<File>,
    boot_id: String,
    spends: Vec<Spend>,
    /// The newest heartbeat per boot, kept through compaction.
    seen: HashMap<String, Entry>,
    exhausted_until_ns: Option<i64>,
}

const MS: i64 = 1_000_000;

impl Ledger {
    /// Load `path` (absent = empty) for the current boot and open it for
    /// appending. `max_window_s` is the longest duty window in force.
    pub fn open(
        path: &Path,
        boot_id: &str,
        now: Now,
        max_window_s: u32,
    ) -> io::Result<(Self, LoadReport)> {
        let mut report = LoadReport::default();
        let mut entries = Vec::new();
        let mut reset = false;
        match File::open(path) {
            Ok(f) => {
                let lines: Vec<String> = BufReader::new(f).lines().collect::<io::Result<_>>()?;
                let last = lines.iter().rposition(|l| !l.trim().is_empty());
                for (i, line) in lines.iter().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Entry>(line) {
                        Ok(e) => entries.push(e),
                        Err(_) if Some(i) == last => report.torn_tail = true,
                        Err(_) => {
                            reset = true;
                            break;
                        }
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut ledger = Ledger {
            path: path.to_path_buf(),
            file: None,
            boot_id: boot_id.to_string(),
            spends: Vec::new(),
            seen: HashMap::new(),
            exhausted_until_ns: None,
        };
        if reset {
            let mut bad = path.as_os_str().to_owned();
            bad.push(".bad");
            std::fs::rename(path, &bad)?;
            report.reset = true;
            ledger.exhausted_until_ns = Some(now.mono_ns + i64::from(max_window_s) * 1000 * MS);
            entries.clear();
        }
        ledger.place(entries, now, &mut report);
        if report.torn_tail {
            // Rewrite without the torn line so the next append starts clean.
            ledger.rewrite()?;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        ledger.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
        Ok((ledger, report))
    }

    fn place(&mut self, entries: Vec<Entry>, now: Now, report: &mut LoadReport) {
        // The latest trusted wall-clock instant each previous boot is known
        // to have been alive at.
        let mut alive: HashMap<&str, i64> = HashMap::new();
        for e in entries.iter().filter(|e| e.utc_trusted) {
            let v = alive.entry(e.boot_id.as_str()).or_insert(e.t_utc_ms);
            *v = (*v).max(e.t_utc_ms);
        }
        let mut spends = Vec::new();
        for e in &entries {
            if e.kind == EntryKind::Seen {
                continue;
            }
            let at_ns = if e.boot_id == self.boot_id {
                e.t_mono_ns
            } else if now.utc_trusted && e.utc_trusted {
                now.mono_ns - (now.utc_ms - e.t_utc_ms).max(0) * MS
            } else if let (true, Some(seen)) = (now.utc_trusted, alive.get(e.boot_id.as_str())) {
                now.mono_ns - (now.utc_ms - seen).max(0) * MS
            } else {
                now.mono_ns
            };
            spends.push(Spend {
                at_ns,
                channel_mhz: e.channel_mhz.unwrap_or(0.0),
                airtime_us: e.airtime_us,
                entry: e.clone(),
            });
        }
        for e in entries {
            if e.kind == EntryKind::Seen {
                let keep = self
                    .seen
                    .get(&e.boot_id)
                    .is_none_or(|old| old.t_mono_ns <= e.t_mono_ns);
                if keep {
                    self.seen.insert(e.boot_id.clone(), e);
                }
            }
        }
        report.entries = spends.len();
        spends.sort_by_key(|s| s.at_ns);
        self.spends = spends;
    }

    fn append(&mut self, e: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(e).map_err(io::Error::other)?;
        line.push(b'\n');
        let f = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("ledger not open"))?;
        f.write_all(&line)?;
        f.sync_data()
    }

    /// Persist a charge, then account it. Called before the frame is sent.
    pub fn charge(
        &mut self,
        now: Now,
        channel_mhz: f64,
        sub_band: &str,
        airtime_us: u64,
        plan_id: &str,
    ) -> io::Result<()> {
        let e = Entry {
            kind: EntryKind::Tx,
            boot_id: self.boot_id.clone(),
            t_mono_ns: now.mono_ns,
            t_utc_ms: now.utc_ms,
            utc_trusted: now.utc_trusted,
            sub_band: Some(sub_band.to_string()),
            channel_mhz: Some(channel_mhz),
            airtime_us,
            plan_id: Some(plan_id.to_string()),
        };
        self.append(&e)?;
        self.spends.push(Spend {
            at_ns: now.mono_ns,
            channel_mhz,
            airtime_us,
            entry: e,
        });
        Ok(())
    }

    /// Persist a `seen` heartbeat.
    pub fn heartbeat(&mut self, now: Now) -> io::Result<()> {
        let e = Entry {
            kind: EntryKind::Seen,
            boot_id: self.boot_id.clone(),
            t_mono_ns: now.mono_ns,
            t_utc_ms: now.utc_ms,
            utc_trusted: now.utc_trusted,
            sub_band: None,
            channel_mhz: None,
            airtime_us: 0,
            plan_id: None,
        };
        self.append(&e)?;
        self.seen.insert(self.boot_id.clone(), e);
        Ok(())
    }

    /// Every spend, oldest first.
    pub fn spends(&self) -> &[Spend] {
        &self.spends
    }

    /// Charged airtime of spends on channels in `lo..=hi` MHz at or after
    /// `since_ns`.
    pub fn spent_in(&self, lo_mhz: f64, hi_mhz: f64, since_ns: i64) -> u64 {
        self.spends
            .iter()
            .filter(|s| s.at_ns > since_ns && s.channel_mhz >= lo_mhz && s.channel_mhz <= hi_mhz)
            .map(|s| s.airtime_us)
            .sum()
    }

    /// While set and in the future, every duty rule counts as exhausted (the
    /// ledger was corrupt and its spend unknown).
    pub fn exhausted_until_ns(&self) -> Option<i64> {
        self.exhausted_until_ns
    }

    /// Drop spends older than `max_window_s` and rewrite the file atomically.
    pub fn compact(&mut self, now: Now, max_window_s: u32) -> io::Result<()> {
        let cutoff = now.mono_ns - i64::from(max_window_s) * 1000 * MS;
        self.spends.retain(|s| s.at_ns > cutoff);
        self.rewrite()?;
        self.file = Some(OpenOptions::new().append(true).open(&self.path)?);
        Ok(())
    }

    fn rewrite(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut f = File::create(&tmp)?;
            let seen = self.seen.values();
            for e in seen.chain(self.spends.iter().map(|s| &s.entry)) {
                let mut line = serde_json::to_vec(e).map_err(io::Error::other)?;
                line.push(b'\n');
                f.write_all(&line)?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests;
