//! Scheduler-pressure counters: page faults from `/proc/vmstat`.
//!
//! Context switches (`ctxt`) and forks (`processes`) come from `/proc/stat`
//! (parsed in [`super::cpu`]); the page-fault counters come from `/proc/vmstat`:
//! `pgfault` (total minor + major faults) and `pgmajfault` (major faults that
//! hit storage). All three are cumulative; their rates are derived at read time
//! from successive snapshots, so the raw values are recorded.

use std::path::Path;

use super::reader::under;

/// Cumulative page-fault counters from `/proc/vmstat`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmStat {
    /// `pgfault`: cumulative total page faults.
    pub pgfault: Option<u64>,
    /// `pgmajfault`: cumulative major page faults.
    pub pgmajfault: Option<u64>,
}

/// Read and parse `/proc/vmstat`. Returns a default (all-`None`) [`VmStat`] when
/// the file is absent.
pub fn read_vmstat(root: &Path) -> VmStat {
    let path = under(root, "/proc/vmstat");
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_vmstat(&text),
        Err(_) => VmStat::default(),
    }
}

/// Parse the text of `/proc/vmstat`. Each line is `<key> <value>`; only the
/// page-fault keys are extracted.
pub fn parse_vmstat(text: &str) -> VmStat {
    let mut out = VmStat::default();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let value = parts.next().and_then(|v| v.parse::<u64>().ok());
        match key {
            "pgfault" => out.pgfault = value,
            "pgmajfault" => out.pgmajfault = value,
            _ => {}
        }
    }
    out
}

/// The 1/5/15-minute load averages from `/proc/loadavg`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadAvg {
    /// 1-minute load average.
    pub one: f64,
    /// 5-minute load average.
    pub five: f64,
    /// 15-minute load average.
    pub fifteen: f64,
}

/// Read and parse `/proc/loadavg`. `None` when the file is absent or malformed.
pub fn read_loadavg(root: &Path) -> Option<LoadAvg> {
    let path = under(root, "/proc/loadavg");
    let text = std::fs::read_to_string(&path).ok()?;
    parse_loadavg(&text)
}

/// Parse `/proc/loadavg`: `<1m> <5m> <15m> <running>/<total> <lastpid>`. Returns
/// `None` unless the first three whitespace-separated fields parse as floats.
pub fn parse_loadavg(text: &str) -> Option<LoadAvg> {
    let mut f = text.split_whitespace();
    let one = f.next()?.parse::<f64>().ok()?;
    let five = f.next()?.parse::<f64>().ok()?;
    let fifteen = f.next()?.parse::<f64>().ok()?;
    Some(LoadAvg { one, five, fifteen })
}

#[cfg(test)]
mod tests;
