//! RTK base state machine: `off → surveying → fixed | failed`.
//!
//! The machine is pure: [`Base::apply`] returns the bytes to write to the
//! receiver, and receiver reports (NAV-SVIN, ACK, MON-VER) advance it. Nothing
//! is reported as progress until the receiver itself reports it.

use serde::Serialize;

use crate::ubx::NavSvin;
use crate::valset::{self, RxPort};

/// How long the receiver has to confirm a base command (survey-in running,
/// or the fixed-mode VALSET acknowledged).
pub const CONFIRM_TIMEOUT_NS: u64 = 10_000_000_000;

/// Failure reason recorded when the receiver cannot act as a base.
pub const E_UNSUPPORTED: &str = "E_GNSS_UNSUPPORTED";

/// A requested base mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BaseMode {
    /// Not a base.
    Off,
    /// Survey-in until both the minimum duration and the accuracy are met.
    Survey {
        /// Minimum survey duration, seconds.
        min_dur_s: u32,
        /// Target mean position accuracy, metres.
        accuracy_m: f64,
    },
    /// A known base position.
    Fixed(Llh),
}

impl BaseMode {
    /// `off`, `survey` or `fixed`.
    pub fn as_str(&self) -> &'static str {
        match self {
            BaseMode::Off => "off",
            BaseMode::Survey { .. } => "survey",
            BaseMode::Fixed(_) => "fixed",
        }
    }
}

/// Geodetic position, WGS84.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Llh {
    /// Latitude, degrees.
    pub lat_deg: f64,
    /// Longitude, degrees.
    pub lon_deg: f64,
    /// Height above the ellipsoid, metres.
    pub h_ellipsoid_m: f64,
}

/// Base machine state, serialized in snake case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseState {
    /// Not a base.
    Off,
    /// Survey-in running.
    Surveying,
    /// Base position fixed (surveyed or configured).
    Fixed,
    /// The receiver rejected or did not confirm the command.
    Failed,
}

/// What the base receiver can do.
#[derive(Debug, Clone, Default)]
pub struct Capability {
    /// Configuration bytes can reach the receiver.
    pub writable: bool,
    /// Model string, when known (config or MON-VER).
    pub model: Option<String>,
    /// Receiver port the host is attached to.
    pub port: RxPort,
}

/// Modules with the RTK base function (CFG-TMODE survey-in and fixed mode
/// plus RTCM3 base output), as the MON-VER `MOD=` extension names them. The
/// ZED-F9R (a sensor-fusion rover) and the ZED-F9T (a timing receiver whose
/// time mode emits no RTCM3 base messages) are not bases.
pub const BASE_MODULES: [&str; 3] = ["ZED-F9P", "NEO-F9P", "ZED-X20P"];

/// A model string (`u-blox ZED-F9P`, or the bare `ZED-F9P`) names a module in
/// [`BASE_MODULES`]: one whitespace-separated word must equal an entry,
/// ignoring case.
pub fn survey_capable(model: &str) -> bool {
    model
        .split_whitespace()
        .any(|w| BASE_MODULES.iter().any(|m| m.eq_ignore_ascii_case(w)))
}

/// The base machine.
#[derive(Debug, Clone)]
pub struct Base {
    mode: BaseMode,
    state: BaseState,
    reason: Option<String>,
    commanded_ns: Option<u64>,
    confirmed: bool,
    elapsed_s: u32,
    accuracy_m: Option<f64>,
    fixed: Option<Llh>,
    min_dur_s: u32,
    target_accuracy_m: f64,
}

impl Base {
    /// An idle machine with the configured survey defaults.
    pub fn new(min_dur_s: u32, target_accuracy_m: f64) -> Self {
        Self {
            mode: BaseMode::Off,
            state: BaseState::Off,
            reason: None,
            commanded_ns: None,
            confirmed: false,
            elapsed_s: 0,
            accuracy_m: None,
            fixed: None,
            min_dur_s,
            target_accuracy_m,
        }
    }

    /// The requested mode.
    pub fn mode(&self) -> BaseMode {
        self.mode
    }
    /// The machine state.
    pub fn state(&self) -> BaseState {
        self.state
    }
    /// Why the machine is `failed`.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
    /// The survey defaults `(min_dur_s, target_accuracy_m)`.
    pub fn defaults(&self) -> (u32, f64) {
        (self.min_dur_s, self.target_accuracy_m)
    }
    /// True while this receiver acts as a base (its RTCM3 output is forwarded).
    pub fn active(&self) -> bool {
        !matches!(self.mode, BaseMode::Off) && self.state != BaseState::Failed
    }

    /// The receiver confirmed the current command (a running survey-in
    /// report, or the fixed-mode acknowledgement).
    pub fn confirmed(&self) -> bool {
        self.confirmed && self.state != BaseState::Failed
    }

    fn fail(&mut self, reason: String) {
        tracing::warn!(reason = %reason, mode = self.mode.as_str(), "gnss_base_failed");
        self.state = BaseState::Failed;
        self.reason = Some(reason);
        self.commanded_ns = None;
    }

    /// Record the configured mode while the receiver port is not open yet.
    /// Nothing is claimed: the state stays `off` until [`Base::apply`]
    /// commands the receiver.
    pub fn request(&mut self, mode: BaseMode) {
        self.reset(mode);
    }

    fn reset(&mut self, mode: BaseMode) {
        self.mode = mode;
        self.reason = None;
        self.confirmed = false;
        self.elapsed_s = 0;
        self.accuracy_m = None;
        self.fixed = None;
        self.commanded_ns = None;
        self.state = BaseState::Off;
        if let BaseMode::Survey {
            min_dur_s,
            accuracy_m,
        } = mode
        {
            self.min_dur_s = min_dur_s;
            self.target_accuracy_m = accuracy_m;
        }
    }

    /// Apply a requested mode. `Ok(bytes)` are to be written to the receiver
    /// (none when it cannot be written and none are needed); `Err` is the
    /// error code and detail of a refused request, which also leaves the
    /// machine `failed`.
    pub fn apply(
        &mut self,
        mode: BaseMode,
        cap: &Capability,
        now: u64,
    ) -> Result<Option<Vec<u8>>, (&'static str, String)> {
        self.reset(mode);
        if mode == BaseMode::Off {
            return Ok(cap.writable.then(|| valset::off(cap.port)));
        }
        if !cap.writable {
            let detail = "the receiver is attached through the sensor hub and cannot be configured";
            self.fail(detail.to_string());
            return Err((E_UNSUPPORTED, detail.to_string()));
        }
        if let Some(model) = cap.model.as_deref().filter(|m| !survey_capable(m)) {
            let detail = format!("receiver model {model} has no base mode");
            self.fail(detail.clone());
            return Err((E_UNSUPPORTED, detail));
        }
        self.commanded_ns = Some(now);
        let bytes = match mode {
            BaseMode::Survey {
                min_dur_s,
                accuracy_m,
            } => {
                self.state = BaseState::Surveying;
                valset::survey_in(cap.port, min_dur_s, accuracy_m)
            }
            BaseMode::Fixed(p) => valset::fixed(cap.port, p.lat_deg, p.lon_deg, p.h_ellipsoid_m),
            BaseMode::Off => unreachable!("handled above"),
        };
        Ok(Some(bytes))
    }

    /// The receiver reported its model (MON-VER).
    pub fn on_model(&mut self, model: &str) {
        if self.mode != BaseMode::Off && self.state != BaseState::Failed && !survey_capable(model) {
            self.fail(format!("receiver model {model} has no base mode"));
        }
    }

    /// NAV-SVIN from the base receiver.
    pub fn on_svin(&mut self, s: &NavSvin) {
        if !matches!(self.mode, BaseMode::Survey { .. }) || self.state != BaseState::Surveying {
            return;
        }
        if !(s.active || s.valid) {
            return;
        }
        self.confirmed = true;
        self.elapsed_s = s.dur_s;
        self.accuracy_m = Some(s.mean_acc_m);
        if s.valid {
            self.state = BaseState::Fixed;
            self.fixed = Some(ecef_to_llh(s.mean_ecef_m));
        }
    }

    /// ACK-ACK (`ok`) or ACK-NAK for a CFG-VALSET.
    pub fn on_valset_ack(&mut self, ok: bool) {
        if self.commanded_ns.is_none() || self.state == BaseState::Failed {
            return;
        }
        if !ok {
            self.fail("the receiver rejected the base configuration".to_string());
            return;
        }
        if let BaseMode::Fixed(p) = self.mode {
            self.confirmed = true;
            self.state = BaseState::Fixed;
            self.fixed = Some(p);
        }
    }

    /// Fail a command the receiver never confirmed.
    pub fn tick(&mut self, now: u64) {
        let Some(t) = self.commanded_ns else {
            return;
        };
        if self.confirmed || self.state == BaseState::Failed {
            return;
        }
        if now.saturating_sub(t) > CONFIRM_TIMEOUT_NS {
            let reason = match self.mode {
                BaseMode::Survey { .. } => "no survey-in report (NAV-SVIN) from the receiver",
                _ => "the receiver did not acknowledge the base configuration",
            };
            self.fail(reason.to_string());
        }
    }

    /// The `base` object minus `rtcm_out`.
    pub fn view(&self) -> BaseView {
        BaseView {
            mode: self.mode.as_str(),
            state: self.state,
            elapsed_s: self.elapsed_s,
            min_dur_s: self.min_dur_s,
            accuracy_m: self.accuracy_m,
            target_accuracy_m: self.target_accuracy_m,
            fixed: self.fixed.filter(|_| self.state == BaseState::Fixed),
        }
    }
}

/// A serializable snapshot of the base machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BaseView {
    /// Mode name, as [`BaseMode::as_str`].
    pub mode: &'static str,
    /// Machine state.
    pub state: BaseState,
    /// Survey-in elapsed time, seconds.
    pub elapsed_s: u32,
    /// Survey-in minimum duration, seconds.
    pub min_dur_s: u32,
    /// Current survey-in accuracy, metres.
    pub accuracy_m: Option<f64>,
    /// Target survey-in accuracy, metres.
    pub target_accuracy_m: f64,
    /// The base position, once fixed.
    pub fixed: Option<Llh>,
}

/// WGS84 ECEF (m) to geodetic (Bowring, iterated).
pub fn ecef_to_llh([x, y, z]: [f64; 3]) -> Llh {
    const A: f64 = 6_378_137.0;
    const F: f64 = 1.0 / 298.257_223_563;
    let e2 = F * (2.0 - F);
    let p = (x * x + y * y).sqrt();
    let lon = y.atan2(x);
    let mut lat = z.atan2(p * (1.0 - e2));
    let mut h = 0.0;
    for _ in 0..8 {
        let s = lat.sin();
        let n = A / (1.0 - e2 * s * s).sqrt();
        h = if lat.cos().abs() > 1e-9 {
            p / lat.cos() - n
        } else {
            z.abs() - n * (1.0 - e2)
        };
        lat = z.atan2(p * (1.0 - e2 * n / (n + h)));
    }
    Llh {
        lat_deg: lat.to_degrees(),
        lon_deg: lon.to_degrees(),
        h_ellipsoid_m: h,
    }
}

#[cfg(test)]
mod tests;
