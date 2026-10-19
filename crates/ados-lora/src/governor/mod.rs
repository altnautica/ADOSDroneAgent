//! The transmit governor: every frame passes [`Governor::admit`] before it
//! reaches a radio.
//!
//! Checks run in a fixed order and the first refusal wins: region resolved ->
//! module band -> `max_power` -> `dwell_time` -> `duty_cycle` -> `lbt`. The
//! first five are decided here against the plan and the ledger; listen-before-
//! talk needs the radio, so the daemon runs it on the granted channel and
//! reports the outcome ([`Governor::commit`] or [`Governor::refuse`]). A
//! commit charges the ledger (fsync'd) before the frame is sent.

pub mod lbt;
pub mod power;
pub mod rules;
mod status;

use std::collections::BTreeMap;
use std::io;

use crate::airtime::{time_on_air_us, Modem};
use crate::band::{self, ModuleCaps};
use crate::ledger::{Ledger, Now};
use crate::regions::{Plan, Resolved, SubBand};
use power::{Antenna, Apc};

pub use status::GovernorStatus;

/// Why a frame was not transmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    RegionUnset,
    ModuleBandMismatch,
    PowerOverLimit,
    DwellTime,
    DutyExhausted,
    LbtBusy,
}

impl Refusal {
    /// The `refused_by_rule` key.
    pub fn rule(self) -> &'static str {
        match self {
            Refusal::RegionUnset => "region_unset",
            Refusal::ModuleBandMismatch => "module_band_mismatch",
            Refusal::PowerOverLimit => "max_power",
            Refusal::DwellTime => "dwell_time",
            Refusal::DutyExhausted => "duty_cycle",
            Refusal::LbtBusy => "lbt",
        }
    }

    /// The `blocked_reason` value.
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::RegionUnset => "region_unset",
            Refusal::ModuleBandMismatch => "module_band_mismatch",
            Refusal::PowerOverLimit => "power_over_limit",
            Refusal::DwellTime => "dwell_time",
            Refusal::DutyExhausted => "duty_exhausted",
            Refusal::LbtBusy => "lbt_busy",
        }
    }
}

/// Every `refused_by_rule` key, in display order.
pub const RULE_KEYS: [&str; 6] = [
    "duty_cycle",
    "dwell_time",
    "lbt",
    "max_power",
    "region_unset",
    "module_band_mismatch",
];

/// What the governor is set up with.
#[derive(Debug, Clone)]
pub struct Setup {
    pub resolved: Option<Resolved>,
    pub caps: ModuleCaps,
    pub modem: Modem,
    /// Multiplier on time on air charged to the ledger.
    pub airtime_factor: u64,
    /// Fixed channel centre, else the plan default (or hop).
    pub freq_mhz: Option<f64>,
    /// Requested ERP, else the plan clamp.
    pub erp_mw: Option<f64>,
    pub antenna: Antenna,
    /// Seeds the hop permutation.
    pub node_id: u16,
}

/// An admitted frame's channel, power and charge.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub channel_mhz: f64,
    pub power_dbm: i8,
    /// Charged airtime (time on air x the transport factor), microseconds.
    pub airtime_us: u64,
    pub sub_band: String,
    pub plan_id: &'static str,
}

#[derive(Debug, Default, Clone)]
struct Counters {
    frames: u64,
    refused: u64,
    by_rule: BTreeMap<&'static str, u64>,
}

pub struct Governor {
    setup: Setup,
    ledger: Ledger,
    channels: Vec<f64>,
    hop_order: Vec<usize>,
    hop_pos: usize,
    static_block: Option<Refusal>,
    counters: Counters,
    blocked: Option<Refusal>,
    last_refusal: Option<(Refusal, i64)>,
    last_grant: Option<Grant>,
    lbt_last_rssi: Option<i16>,
    lbt_busy_count: u64,
    apc: Apc,
    reported_air_util_pct: Option<f64>,
}

/// A pseudo-random permutation of `0..n` seeded by the node id, so nodes
/// sharing a plan spread over different channels.
fn hop_permutation(n: usize, seed: u16) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    let mut x = u64::from(seed) ^ 0x9E37_79B9_7F4A_7C15;
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        order.swap(i, (x % (i as u64 + 1)) as usize);
    }
    order
}

impl Governor {
    pub fn new(setup: Setup, ledger: Ledger) -> Self {
        let (channels, static_block) = match setup.resolved {
            None => (Vec::new(), Some(Refusal::RegionUnset)),
            Some(r) => Self::plan_channels(&setup, r.plan),
        };
        let hop_order = hop_permutation(channels.len(), setup.node_id);
        Self {
            setup,
            ledger,
            channels,
            hop_order,
            hop_pos: 0,
            static_block,
            counters: Counters::default(),
            blocked: None,
            last_refusal: None,
            last_grant: None,
            lbt_last_rssi: None,
            lbt_busy_count: 0,
            apc: Apc::default(),
            reported_air_util_pct: None,
        }
    }

    /// The channels frames may use and the standing block, if any. The module
    /// band is a hardware fact and is reported before a research-only row.
    fn plan_channels(setup: &Setup, plan: &Plan) -> (Vec<f64>, Option<Refusal>) {
        let bw = setup.modem.bw_khz;
        if !band::supported(plan, &setup.caps, bw) || !plan.bandwidth_ok(bw) {
            return (Vec::new(), Some(Refusal::ModuleBandMismatch));
        }
        if !plan.tx {
            return (Vec::new(), Some(Refusal::RegionUnset));
        }
        let all = band::candidates(plan, &setup.caps, bw);
        let hopping = plan.hop && setup.caps.hops();
        let fixed = match setup.freq_mhz {
            Some(f) => Some(f),
            None if hopping => None,
            // `supported` guarantees at least one candidate.
            None => Some(
                plan.default_mhz
                    .filter(|f| all.iter().any(|c| (c - f).abs() < 1e-6))
                    .unwrap_or(all[0]),
            ),
        };
        match fixed {
            Some(f) if band::channel_ok(plan, &setup.caps, f, bw) => (vec![f], None),
            Some(_) => (Vec::new(), Some(Refusal::ModuleBandMismatch)),
            None => (all, None),
        }
    }

    pub fn plan(&self) -> Option<&'static Plan> {
        self.setup.resolved.map(|r| r.plan)
    }

    pub fn setup(&self) -> &Setup {
        &self.setup
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// The standing block no wait clears (`region_unset`,
    /// `module_band_mismatch`).
    pub fn static_block(&self) -> Option<Refusal> {
        self.static_block
    }

    /// Charged airtime of a `len`-byte frame.
    pub fn charge_us(&self, len: usize) -> u64 {
        time_on_air_us(&self.setup.modem, len) * self.setup.airtime_factor
    }

    pub fn set_reported_air_util(&mut self, pct: Option<f64>) {
        self.reported_air_util_pct = pct;
    }

    pub fn note_peer_report(&mut self, peer: u16, snr_db: f32, now: Now) {
        self.apc.note_report(peer, snr_db, now.mono_ns);
    }

    /// Re-run adaptive power control (plans that require it), once per beacon.
    pub fn apc_tick(&mut self, now: Now, interval_s: u32) {
        if self.plan().is_some_and(|p| p.apc) {
            let max = self.setup.caps.power_steps_dbm.len().saturating_sub(1);
            let floor = self.setup.modem.snr_floor_db();
            self.apc.update(
                now.mono_ns,
                i64::from(interval_s) * 1_000_000_000,
                floor,
                max,
            );
        }
    }

    fn power_for(&self, sb: &SubBand) -> Option<i8> {
        let rule = sb.power?;
        let mut cap = power::conducted_cap_dbm(&rule, &self.setup.antenna, self.setup.modem.bw_khz);
        if let Some(erp) = self.setup.erp_mw {
            let want = self.setup.antenna.conducted_for(
                power::mw_to_dbm(erp),
                crate::regions::PowerRef::Erp,
                None,
            );
            cap = cap.min(want);
        }
        let down = if self.plan().is_some_and(|p| p.apc) {
            self.apc.steps_down
        } else {
            0
        };
        power::pick_step(&self.setup.caps.power_steps_dbm, cap, down)
    }

    /// Channels to try, in order: the fixed one, or the hop sequence from the
    /// current position.
    fn order(&self, override_mhz: Option<f64>) -> Vec<f64> {
        if let Some(f) = override_mhz {
            return vec![f];
        }
        let n = self.channels.len();
        (0..n)
            .map(|i| self.channels[self.hop_order[(self.hop_pos + i) % n]])
            .collect()
    }

    /// Decide a `len`-byte frame. `channel_override` pins the channel (it
    /// must be a usable channel of the plan). A refusal is recorded.
    pub fn admit(
        &mut self,
        len: usize,
        channel_override: Option<f64>,
        now: Now,
    ) -> Result<Grant, Refusal> {
        let r = self.decide(len, channel_override, now);
        if let Err(refusal) = r {
            self.refuse(refusal, now);
        }
        r
    }

    fn decide(
        &self,
        len: usize,
        channel_override: Option<f64>,
        now: Now,
    ) -> Result<Grant, Refusal> {
        if let Some(block) = self.static_block {
            return Err(block);
        }
        let plan = self.plan().ok_or(Refusal::RegionUnset)?;
        let airtime = self.charge_us(len);
        let bw = self.setup.modem.bw_khz;
        if let Some(f) = channel_override {
            if !band::channel_ok(plan, &self.setup.caps, f, bw) {
                return Err(Refusal::ModuleBandMismatch);
            }
        }
        let with_power: Vec<(f64, &SubBand, i8)> = self
            .order(channel_override)
            .into_iter()
            .filter_map(|f| {
                let sb = plan.sub_band_for(f, bw)?;
                Some((f, sb, self.power_for(sb)?))
            })
            .collect();
        if with_power.is_empty() {
            return Err(Refusal::PowerOverLimit);
        }
        let dwell_ok: Vec<_> = match plan.dwell {
            None => with_power,
            Some(rule) => with_power
                .into_iter()
                .filter(|(f, _, _)| {
                    rules::dwell_wait_ns(&self.ledger, &rule, *f, airtime, now.mono_ns) == Some(0)
                })
                .collect(),
        };
        let reported = self.reported_air_util_pct;
        let (f, sb, p) = dwell_ok
            .iter()
            .copied()
            .find(|(_, sb, _)| match sb.duty {
                None => true,
                Some(rule) => {
                    rules::duty_wait_ns(&self.ledger, sb, &rule, airtime, now.mono_ns, reported)
                        == Some(0)
                }
            })
            .ok_or(if dwell_ok.is_empty() {
                Refusal::DwellTime
            } else {
                Refusal::DutyExhausted
            })?;
        Ok(Grant {
            channel_mhz: f,
            power_dbm: p,
            airtime_us: airtime,
            sub_band: sb.label(),
            plan_id: plan.plan_id,
        })
    }

    /// Charge an admitted frame to the ledger; call after listen-before-talk
    /// passed and before the frame is sent. The frame is counted as sent only
    /// by [`Governor::note_sent`], since a re-listen may still refuse it.
    pub fn commit(&mut self, grant: &Grant, now: Now) -> io::Result<()> {
        self.ledger.charge(
            now,
            grant.channel_mhz,
            &grant.sub_band,
            grant.airtime_us,
            grant.plan_id,
        )?;
        self.blocked = None;
        if !self.channels.is_empty() {
            self.hop_pos = (self.hop_pos + 1) % self.channels.len();
        }
        self.last_grant = Some(grant.clone());
        Ok(())
    }

    /// Count a frame that reached the radio.
    pub fn note_sent(&mut self) {
        self.counters.frames += 1;
    }

    /// Record a refusal (the daemon calls this for `lbt_busy`).
    pub fn refuse(&mut self, refusal: Refusal, now: Now) {
        self.counters.refused += 1;
        *self.counters.by_rule.entry(refusal.rule()).or_default() += 1;
        self.last_refusal = Some((refusal, now.utc_ms));
        self.blocked = Some(refusal);
        tracing::info!(
            rule = refusal.rule(),
            plan = self.plan().map_or("none", |p| p.plan_id),
            "lora.tx_refused"
        );
    }

    /// Record one listen-before-talk outcome.
    pub fn note_lbt(&mut self, last_rssi_dbm: Option<i16>, busy: bool) {
        self.lbt_last_rssi = last_rssi_dbm.or(self.lbt_last_rssi);
        if busy {
            self.lbt_busy_count += 1;
        }
    }

    /// Time until a `len`-byte frame would be admitted, in ms (0 = now, and
    /// 0 under a standing block, which no wait clears).
    pub fn next_tx_allowed_ms(&self, len: usize, now: Now) -> u64 {
        let Some(plan) = self.plan().filter(|_| self.static_block.is_none()) else {
            return 0;
        };
        let airtime = self.charge_us(len);
        let bw = self.setup.modem.bw_khz;
        let waits = self.channels.iter().filter_map(|f| {
            let sb = plan.sub_band_for(*f, bw)?;
            self.power_for(sb)?;
            let dwell = match plan.dwell {
                Some(rule) => rules::dwell_wait_ns(&self.ledger, &rule, *f, airtime, now.mono_ns)?,
                None => 0,
            };
            let duty = match sb.duty {
                Some(rule) => rules::duty_wait_ns(
                    &self.ledger,
                    sb,
                    &rule,
                    airtime,
                    now.mono_ns,
                    self.reported_air_util_pct,
                )?,
                None => 0,
            };
            Some(dwell.max(duty))
        });
        waits
            .min()
            .map_or(0, |ns| (ns.max(0) as u64).div_ceil(1_000_000))
    }

    /// The channel the status reports: the last one used, else the first.
    pub fn current_channel(&self) -> Option<f64> {
        self.last_grant
            .as_ref()
            .map(|g| g.channel_mhz)
            .or_else(|| self.channels.first().copied())
    }

    /// Compact the ledger to the longest duty window of the plan.
    pub fn compact(&mut self, now: Now) -> io::Result<()> {
        self.ledger.compact(now, max_window_s(self.plan()))
    }
}

/// The longest duty window of `plan` (3600 s when it has none, which also
/// covers every dwell period).
pub fn max_window_s(plan: Option<&Plan>) -> u32 {
    plan.into_iter()
        .flat_map(|p| p.sub_bands.iter())
        .filter_map(|s| s.duty.map(|d| d.window_s))
        .max()
        .unwrap_or(3600)
}

#[cfg(test)]
mod tests;
