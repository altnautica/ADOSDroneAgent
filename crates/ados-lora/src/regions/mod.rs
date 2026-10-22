//! The compiled regional band-plan table and region resolution.
//!
//! One row per supported region. A row that ships with rules transmits under
//! them; a row whose limits could not all be cited from the regulator text
//! ships with its bands only (`rules: []`) and is receive-only. Every enforced
//! number in the compiled table carries the regulator document it comes from in the
//! row's `citation`.
//!
//! Resolution order: an explicit region code when set, else a country code
//! mapped through [`country_plan`], else none
//! (transmit blocked with `region_unset`, receive still on).
mod country;
mod elrs;
mod research;
mod table;

pub use country::country_plan;
pub use elrs::elrs_domains;

use serde::Serialize;

/// The power reference a limit is published in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerRef {
    /// Effective radiated power (dipole reference).
    Erp,
    /// Effective isotropic radiated power.
    Eirp,
    /// Conducted power at the radio output.
    Conducted,
}

/// A `max_power` rule.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PowerRule {
    /// Power limit, milliwatts.
    pub limit_mw: f64,
    /// What the limit is measured against.
    pub reference: PowerRef,
    /// Power spectral density cap in dBm per 3 kHz, in the same reference.
    pub psd_dbm_per_3khz: Option<f64>,
    /// Antenna gain above which a conducted limit is reduced dB for dB.
    pub antenna_max_dbi: Option<f64>,
}

/// A `duty_cycle` rule: `limit_pct` of airtime over a rolling `window_s`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DutyRule {
    /// Maximum duty, percent of the window.
    pub limit_pct: f64,
    /// Rolling window length, seconds.
    pub window_s: u32,
}

impl DutyRule {
    /// The airtime budget over one window, in microseconds.
    pub fn budget_us(&self) -> u64 {
        (self.limit_pct / 100.0 * f64::from(self.window_s) * 1_000_000.0).round() as u64
    }
}

/// A `dwell_time` rule, per channel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DwellRule {
    /// Occupancy limit of one channel over `per_s` seconds; `None` when the
    /// plan only bounds a single transmission.
    pub limit_ms: Option<u32>,
    /// Window the occupancy limit applies over, seconds.
    pub per_s: Option<u32>,
    /// Longest single transmission.
    pub single_tx_max_ms: u32,
    /// Minimum pause between transmissions on one channel.
    pub pause_ms: u32,
}

/// A `lbt` (listen before talk) rule.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LbtRule {
    /// Busy threshold, dBm.
    pub threshold_dbm: i16,
    /// Minimum listen time, microseconds.
    pub listen_us: u32,
    /// Maximum gap between listen and transmit, microseconds.
    pub gap_max_us: u32,
}

/// A channel grid: centres at `start_mhz + n * step_mhz`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Grid {
    /// First channel centre, MHz.
    pub start_mhz: f64,
    /// Channel spacing, MHz.
    pub step_mhz: f64,
}

/// One sub-band of a plan with the limits that apply inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubBand {
    /// Lower band edge, MHz.
    pub lo_mhz: f64,
    /// Upper band edge, MHz.
    pub hi_mhz: f64,
    /// Duty-cycle rule for this sub-band, if any.
    pub duty: Option<DutyRule>,
    /// `None` only on research rows, which never transmit.
    pub power: Option<PowerRule>,
}

impl SubBand {
    /// The `"lo-hi"` label used in the status and the ledger (`865.0-867.0`).
    pub fn label(&self) -> String {
        format!("{}-{}", fmt_mhz(self.lo_mhz), fmt_mhz(self.hi_mhz))
    }

    /// Whether a carrier of `bw_khz` centred on `mhz` sits wholly inside.
    pub fn fits(&self, mhz: f64, bw_khz: u32) -> bool {
        let half = f64::from(bw_khz) / 2000.0;
        mhz - half >= self.lo_mhz - 1e-9 && mhz + half <= self.hi_mhz + 1e-9
    }
}

/// `865.0`, `869.65`: at least one decimal, no trailing zeros beyond it.
fn fmt_mhz(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0');
    if s.ends_with('.') {
        format!("{s}0")
    } else {
        s.to_string()
    }
}

/// One compiled region row.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The region code (`IN`, `EU`, `EU433`, ...).
    pub code: &'static str,
    /// Human-readable region name.
    pub name: &'static str,
    /// Band-plan identifier.
    pub plan_id: &'static str,
    /// The span the plan's sub-bands cover, for display.
    pub band_mhz: (f64, f64),
    /// The regulator documents every enforced number comes from.
    pub citation: &'static str,
    /// The sub-bands the plan allows.
    pub sub_bands: &'static [SubBand],
    /// Dwell-time rule, if any.
    pub dwell: Option<DwellRule>,
    /// Listen-before-talk rule, if any.
    pub lbt: Option<LbtRule>,
    /// The plan's mandatory channel grid (JP, KR).
    pub grid: Option<Grid>,
    /// Transmissions hop across every usable channel.
    pub hop: bool,
    /// Minimum number of hop channels the plan requires.
    pub hop_channels_min: u32,
    /// Default channel centre when the operator sets none.
    pub default_mhz: Option<f64>,
    /// Largest occupied bandwidth the plan admits.
    pub bw_max_khz: Option<u32>,
    /// Adaptive power control required.
    pub apc: bool,
    /// The Meshtastic region preset that sits inside this plan, or `None`
    /// when the Meshtastic bridge is disabled under it.
    pub meshtastic: Option<&'static str>,
    /// `false` for a research-only row: listed, never transmits.
    pub tx: bool,
}

/// The rule kinds the governor enforces.
pub const RULE_DUTY: &str = "duty_cycle";
/// Rule key for the dwell-time limit.
pub const RULE_DWELL: &str = "dwell_time";
/// Rule key for listen-before-talk.
pub const RULE_LBT: &str = "lbt";
/// Rule key for the power limit.
pub const RULE_POWER: &str = "max_power";

impl Plan {
    /// The rule kinds this plan declares, in governor order. Empty for a
    /// research-only row.
    pub fn rules(&self) -> Vec<&'static str> {
        if !self.tx {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.sub_bands.iter().any(|s| s.power.is_some()) {
            out.push(RULE_POWER);
        }
        if self.dwell.is_some() {
            out.push(RULE_DWELL);
        }
        if self.sub_bands.iter().any(|s| s.duty.is_some()) {
            out.push(RULE_DUTY);
        }
        if self.lbt.is_some() {
            out.push(RULE_LBT);
        }
        out
    }

    /// The sub-band a carrier centred on `mhz` fits wholly inside.
    pub fn sub_band_for(&self, mhz: f64, bw_khz: u32) -> Option<&'static SubBand> {
        self.sub_bands.iter().find(|s| s.fits(mhz, bw_khz))
    }

    /// Whether `mhz` sits on the plan's channel grid (always true without one).
    pub fn on_grid(&self, mhz: f64) -> bool {
        match self.grid {
            None => true,
            Some(g) => {
                let n = (mhz - g.start_mhz) / g.step_mhz;
                n >= -1e-6 && (n - n.round()).abs() < 1e-6
            }
        }
    }

    /// Whether `bw_khz` is inside the plan's bandwidth limit.
    pub fn bandwidth_ok(&self, bw_khz: u32) -> bool {
        self.bw_max_khz.is_none_or(|max| bw_khz <= max)
    }
}

/// Every compiled row: the transmitting plans, then the research-only rows.
pub fn all() -> impl Iterator<Item = &'static Plan> {
    table::PLANS.iter().chain(research::RESEARCH.iter())
}

/// The row whose code is exactly `code` (uppercased).
pub fn by_code(code: &str) -> Option<&'static Plan> {
    let code = code.trim().to_ascii_uppercase();
    all().find(|p| p.code == code)
}

/// Where the active region came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RegionSource {
    /// Set explicitly as a region code.
    #[serde(rename = "lora.region")]
    LoraRegion,
    /// Mapped from a country code.
    #[serde(rename = "network.regulatory.region")]
    NetworkRegulatory,
}

impl RegionSource {
    /// Stable string form of the source.
    pub fn as_str(self) -> &'static str {
        match self {
            RegionSource::LoraRegion => "lora.region",
            RegionSource::NetworkRegulatory => "network.regulatory.region",
        }
    }
}

/// A resolved region: the row and where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolved {
    /// The resolved plan.
    pub plan: &'static Plan,
    /// Where the region came from.
    pub source: RegionSource,
}

/// Map a code (a row code or an ISO 3166-1 alpha-2 country) to its row.
pub fn lookup(code: &str) -> Option<&'static Plan> {
    by_code(code).or_else(|| country_plan(code).and_then(by_code))
}

/// Resolve the active region from an explicit region code and a country
/// setting. `None` when neither names a known row.
pub fn resolve(lora_region: Option<&str>, country: Option<&str>) -> Option<Resolved> {
    fn pick(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|s| !s.is_empty())
    }
    if let Some(code) = pick(lora_region) {
        return lookup(code).map(|plan| Resolved {
            plan,
            source: RegionSource::LoraRegion,
        });
    }
    pick(country).and_then(lookup).map(|plan| Resolved {
        plan,
        source: RegionSource::NetworkRegulatory,
    })
}

#[cfg(test)]
mod tests;
