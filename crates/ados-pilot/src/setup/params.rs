//! The flight controller's parameter table as the setup engine sees it:
//! download, verified writes with read-back, and `.param` snapshots.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use ados_protocol::mavlink::ardupilotmega::{
    MavMessage, MavParamType, PARAM_REQUEST_LIST_DATA, PARAM_REQUEST_READ_DATA, PARAM_SET_DATA,
};

/// A write is resent after this long without a matching echo.
const WRITE_RETRY_MS: u64 = 1000;
/// A write fails after this many sends.
const WRITE_TRIES: u8 = 3;
/// A read is resent after this long.
const READ_RETRY_MS: u64 = 1000;
const READ_TRIES: u8 = 3;
/// The full download is re-requested (gap fill by index) after this long idle.
const LIST_IDLE_MS: u64 = 1500;

#[derive(Debug, Clone)]
struct PendingWrite {
    value: f32,
    sent_ms: u64,
    tries: u8,
}

#[derive(Debug, Clone)]
struct PendingRead {
    sent_ms: u64,
    tries: u8,
}

/// Parameters that a snapshot never writes back: identities, counters and
/// calibration-time sensor IDs the FC owns.
fn never_restore(name: &str) -> bool {
    const EXACT: &[&str] = &["FORMAT_VERSION", "SYSID_SW_MREV", "SYSID_SW_TYPE", "GND_ABS_PRESS", "GND_ABS_PRESS2", "GND_ABS_PRESS3"];
    const PREFIX: &[&str] = &["STAT_", "STATS_", "SYS_PARAM_VER", "LND_FLIGHT_T_"];
    EXACT.contains(&name)
        || PREFIX.iter().any(|p| name.starts_with(p))
        || name.ends_with("_DEVID")
        || name.ends_with("_DEV_ID")
        || name.contains("_ID")
            && (name.starts_with("INS_") || name.starts_with("COMPASS_") || name.starts_with("CAL_"))
}

#[derive(Debug, Default, Clone)]
pub struct ParamStore {
    values: HashMap<String, (f32, MavParamType)>,
    by_index: HashMap<u16, String>,
    total: Option<u16>,
    list_requested_ms: Option<u64>,
    last_value_ms: u64,
    writes: BTreeMap<String, PendingWrite>,
    failed: Vec<String>,
    reads: BTreeMap<String, PendingRead>,
    unreadable: Vec<String>,
    /// PX4 encodes integer parameters bytewise in the float field.
    bytewise: bool,
    target: (u8, u8),
}

fn is_int(t: MavParamType) -> bool {
    !matches!(t, MavParamType::MAV_PARAM_TYPE_REAL32 | MavParamType::MAV_PARAM_TYPE_REAL64)
}

impl ParamStore {
    pub fn new() -> Self {
        ParamStore { target: (1, 1), ..Default::default() }
    }

    pub fn set_target(&mut self, sysid: u8, compid: u8, bytewise: bool) {
        self.target = (sysid, compid);
        self.bytewise = bytewise;
    }

    pub fn get(&self, name: &str) -> Option<f32> {
        self.values.get(name).map(|(v, _)| *v)
    }

    pub fn has(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    /// The first of `names` the FC has.
    pub fn first_present<'a>(&self, names: &[&'a str]) -> Option<&'a str> {
        names.iter().copied().find(|n| self.has(n))
    }

    pub fn count(&self) -> usize {
        self.values.len()
    }

    pub fn total(&self) -> Option<u16> {
        self.total
    }

    /// The whole table arrived.
    pub fn complete(&self) -> bool {
        self.total.is_some_and(|t| t > 0 && self.by_index.len() >= t as usize)
    }

    pub fn download_progress(&self) -> f32 {
        match self.total {
            Some(t) if t > 0 => (self.by_index.len() as f32 / t as f32).min(1.0),
            _ => 0.0,
        }
    }

    pub fn request_all(&mut self, out: &mut Vec<MavMessage>, now_ms: u64) {
        self.list_requested_ms = Some(now_ms);
        // A fresh download (after connect or a reboot): count indices anew.
        self.by_index.clear();
        self.total = None;
        self.last_value_ms = now_ms;
        out.push(MavMessage::PARAM_REQUEST_LIST(PARAM_REQUEST_LIST_DATA {
            target_system: self.target.0,
            target_component: self.target.1,
        }));
    }

    /// Queues a read of `name`.
    pub fn read(&mut self, name: &str) {
        self.reads.entry(name.to_string()).or_insert(PendingRead { sent_ms: 0, tries: 0 });
    }

    /// Queues a write. A value already equal is not written.
    pub fn set(&mut self, name: &str, value: f32) {
        if self.get(name).is_some_and(|v| close(v, value)) {
            return;
        }
        self.failed.retain(|n| n != name);
        self.writes.insert(name.to_string(), PendingWrite { value, sent_ms: 0, tries: 0 });
    }

    /// No write or read in flight.
    pub fn idle(&self) -> bool {
        self.writes.is_empty() && self.reads.is_empty()
    }

    pub fn reading(&self, name: &str) -> bool {
        self.reads.contains_key(name)
    }

    /// Writes that ran out of tries since the last [`ParamStore::take_failed`].
    pub fn take_failed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.failed)
    }

    pub fn take_unreadable(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unreadable)
    }

    fn decode(&self, raw: f32, t: MavParamType) -> f32 {
        if self.bytewise && is_int(t) {
            raw.to_bits() as i32 as f32
        } else {
            raw
        }
    }

    pub fn on_msg(&mut self, msg: &MavMessage, now_ms: u64) {
        let MavMessage::PARAM_VALUE(p) = msg else { return };
        let name = p.param_id.to_str().unwrap_or("").trim_end_matches('\0').to_string();
        if name.is_empty() {
            return;
        }
        let value = self.decode(p.param_value, p.param_type);
        self.last_value_ms = now_ms;
        if p.param_count > 0 && p.param_count != u16::MAX {
            self.total = Some(p.param_count);
        }
        if p.param_index != u16::MAX {
            self.by_index.insert(p.param_index, name.clone());
        }
        self.reads.remove(&name);
        if let Some(w) = self.writes.get(&name) {
            if close(w.value, value) {
                self.writes.remove(&name);
            }
        }
        self.values.insert(name, (value, p.param_type));
    }

    pub fn tick(&mut self, out: &mut Vec<MavMessage>, now_ms: u64) {
        // Gap fill: re-request missing indices once the stream goes idle.
        if let (Some(total), Some(_)) = (self.total, self.list_requested_ms) {
            if !self.complete() && now_ms.saturating_sub(self.last_value_ms) > LIST_IDLE_MS {
                let mut asked = 0;
                for i in 0..total {
                    if !self.by_index.contains_key(&i) {
                        out.push(MavMessage::PARAM_REQUEST_READ(PARAM_REQUEST_READ_DATA {
                            param_index: i as i16,
                            target_system: self.target.0,
                            target_component: self.target.1,
                            param_id: "".into(),
                        }));
                        asked += 1;
                        if asked >= 20 {
                            break;
                        }
                    }
                }
                self.last_value_ms = now_ms;
            }
        }
        let mut give_up = Vec::new();
        let bytewise = self.bytewise;
        for (name, w) in self.writes.iter_mut() {
            if w.tries > 0 && now_ms.saturating_sub(w.sent_ms) < WRITE_RETRY_MS {
                continue;
            }
            if w.tries >= WRITE_TRIES {
                give_up.push(name.clone());
                continue;
            }
            let t = self
                .values
                .get(name)
                .map(|(_, t)| *t)
                .unwrap_or(MavParamType::MAV_PARAM_TYPE_REAL32);
            let raw = encode(bytewise, w.value, t);
            out.push(MavMessage::PARAM_SET(PARAM_SET_DATA {
                param_value: raw,
                target_system: self.target.0,
                target_component: self.target.1,
                param_id: name.as_str().into(),
                param_type: t,
            }));
            w.tries += 1;
            w.sent_ms = now_ms;
        }
        for n in give_up {
            self.writes.remove(&n);
            self.failed.push(n);
        }
        let mut lost = Vec::new();
        for (name, r) in self.reads.iter_mut() {
            if r.tries > 0 && now_ms.saturating_sub(r.sent_ms) < READ_RETRY_MS {
                continue;
            }
            if r.tries >= READ_TRIES {
                lost.push(name.clone());
                continue;
            }
            out.push(MavMessage::PARAM_REQUEST_READ(PARAM_REQUEST_READ_DATA {
                param_index: -1,
                target_system: self.target.0,
                target_component: self.target.1,
                param_id: name.as_str().into(),
            }));
            r.tries += 1;
            r.sent_ms = now_ms;
        }
        for n in lost {
            self.reads.remove(&n);
            self.unreadable.push(n);
        }
    }

    /// The table as a Mission Planner `.param` file (`NAME,VALUE` per line).
    pub fn to_param_file(&self, header: &str) -> String {
        let mut names: Vec<&String> = self.values.keys().collect();
        names.sort();
        let mut s = String::new();
        for line in header.lines() {
            let _ = writeln!(s, "# {line}");
        }
        for n in names {
            let (v, _) = self.values[n];
            let _ = writeln!(s, "{n},{}", fmt_value(v));
        }
        s
    }

    /// Queues writes restoring `file` (a `.param` body), optionally only
    /// names `keep` accepts. Returns how many writes were queued.
    pub fn restore(&mut self, file: &str, keep: impl Fn(&str) -> bool) -> usize {
        let mut n = 0;
        for (name, value) in parse_param_file(file) {
            if never_restore(&name) || !keep(&name) || !self.has(&name) {
                continue;
            }
            if self.get(&name).is_some_and(|v| close(v, value)) {
                continue;
            }
            self.set(&name, value);
            n += 1;
        }
        n
    }

    /// Test and engine hook: seed a value as if the FC had sent it.
    pub fn seed(&mut self, name: &str, value: f32) {
        self.values.insert(name.to_string(), (value, MavParamType::MAV_PARAM_TYPE_REAL32));
    }
}

/// Floats compare within the precision a PARAM_VALUE round trip keeps.
pub fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1e-4_f32.max(b.abs() * 1e-5)
}

fn fmt_value(v: f32) -> String {
    if v.fract() == 0.0 && v.abs() < 1e9 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Parses `NAME,VALUE` (or `NAME VALUE` / `NAME\tVALUE`) lines, skipping `#` comments.
pub fn parse_param_file(body: &str) -> Vec<(String, f32)> {
    body.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let mut it = l.split([',', ' ', '\t']).filter(|s| !s.is_empty());
            let name = it.next()?.to_string();
            let value = it.next()?.parse::<f32>().ok()?;
            Some((name, value))
        })
        .collect()
}


/// PX4 carries integer parameters bytewise in PARAM_SET's float field.
fn encode(bytewise: bool, value: f32, t: MavParamType) -> f32 {
    if bytewise && is_int(t) {
        f32::from_bits(value.round() as i32 as u32)
    } else {
        value
    }
}