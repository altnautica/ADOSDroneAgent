//! Config + wanted-domain resolution for the regulatory reconciler.
//!
//! The cadence config (`network.reg_reconciler`) and the wanted GLOBAL domain +
//! rendezvous channel (resolved from the operating-region posture and the
//! `video.wfb` block the radio uses) are parsed here. Pure parsing — unit-tested
//! on every host; the canonical-path reads live in `os`.

use std::time::Duration;

use super::policy::is_forceable_domain;

/// Default reconcile cadence. The monitor pass already runs on its own
/// interval; this gate throttles the reconcile so a fast monitor loop does not
/// shell `iw reg get` more often than needed. 30 s is well inside the window in
/// which a drifted domain would otherwise sit broken.
pub(super) const DEFAULT_TICK_INTERVAL_S: u64 = 30;

/// Default duration after process start during which the reconcile runs at the
/// faster `fast_initial_tick_interval` instead of the steady cadence. The boot
/// sequence is when a foreign baked country is most likely to be (re-)asserted:
/// the radio bring-up and the first-boot bind both re-enter monitor mode and
/// re-churn the injection PHY in the first minute. Converging fast here keeps
/// the global domain from sitting at a foreign country between the bind
/// re-entry and the next steady tick; after the window it settles to the steady
/// cadence (the proven steady-state behavior is unchanged). Measured against the
/// process uptime so a supervisor restart re-arms the fast window.
pub(super) const DEFAULT_FAST_INITIAL_WINDOW_S: u64 = 60;

/// Default reconcile cadence during the fast-initial window. Short enough that a
/// foreign domain asserted by a monitor/bind re-entry is corrected within a few
/// seconds (so the onboard WiFi does not blip), but still a throttle (not a busy
/// loop) so the boot path is not flooded with `iw reg get` shells.
pub(super) const DEFAULT_FAST_INITIAL_TICK_INTERVAL_S: u64 = 5;

/// The default wanted regulatory domain, byte-identical to the radio config's
/// `default_reg_domain`. Permits the home channel (149 / 5745 MHz, U-NII-3,
/// non-DFS) at usable power. Operators override per region in config.
pub(super) const DEFAULT_REG_DOMAIN: &str = "US";

/// The default rendezvous channel, byte-identical to the radio config's
/// `default_channel`. Used as the channel-safety target when the config omits a
/// channel / rendezvous pin.
pub(super) const DEFAULT_CHANNEL: u8 = 149;

/// Configuration for the regulatory reconciler, read from
/// `network.reg_reconciler`. Default-ON so a fresh board keeps its onboard WiFi
/// out of the box; an operator can disable it cleanly if a bespoke regulatory
/// setup ever conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegReconcilerConfig {
    /// Whether the reconciler runs at all. Default true.
    pub enabled: bool,
    /// Minimum spacing between reconcile attempts in steady state.
    pub tick_interval: Duration,
    /// How long after process start the reconcile runs at the faster
    /// `fast_initial_tick` cadence. Default 60 s. A zero disables the fast
    /// window (the reconcile uses the steady cadence from boot).
    pub fast_initial_window: Duration,
    /// The reconcile cadence during the fast-initial window. Default 5 s,
    /// floored at 1 s. Only used while uptime is below `fast_initial_window`.
    pub fast_initial_tick: Duration,
}

impl Default for RegReconcilerConfig {
    fn default() -> Self {
        RegReconcilerConfig {
            enabled: true,
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
            fast_initial_window: Duration::from_secs(DEFAULT_FAST_INITIAL_WINDOW_S),
            fast_initial_tick: Duration::from_secs(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S),
        }
    }
}

impl RegReconcilerConfig {
    /// The effective reconcile cadence given the current process uptime. Inside
    /// the fast-initial window (and when that window is enabled) the faster
    /// cadence applies so a foreign domain asserted by the boot-time monitor /
    /// bind re-entry is corrected within a few seconds; after the window it
    /// settles to the steady cadence (the proven steady-state behavior). Pure so
    /// the schedule is unit-tested without a clock.
    pub fn effective_interval(&self, uptime: Duration) -> Duration {
        if !self.fast_initial_window.is_zero() && uptime < self.fast_initial_window {
            self.fast_initial_tick
        } else {
            self.tick_interval
        }
    }
}

/// Parse `network.reg_reconciler` out of a config body. An absent section reads
/// as the all-defaults (enabled) config so the reconciler is on out of the box.
/// A malformed config also falls back to enabled rather than silently disabling
/// the onboard-WiFi protection.
pub fn read_config_from(text: &str) -> RegReconcilerConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        reg_reconciler: Option<Recon>,
    }
    #[derive(serde::Deserialize)]
    struct Recon {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        tick_interval_s: Option<u64>,
        #[serde(default)]
        fast_initial_window_s: Option<u64>,
        #[serde(default)]
        fast_initial_tick_interval_s: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    match serde_norway::from_str::<Raw>(text) {
        Ok(raw) => match raw.network.reg_reconciler {
            Some(r) => RegReconcilerConfig {
                enabled: r.enabled,
                // Floor at 1 s so a zero in config cannot spin the reconcile.
                tick_interval: Duration::from_secs(
                    r.tick_interval_s.unwrap_or(DEFAULT_TICK_INTERVAL_S).max(1),
                ),
                // A zero window is honored (disables the fast convergence); any
                // positive value is taken as-is.
                fast_initial_window: Duration::from_secs(
                    r.fast_initial_window_s
                        .unwrap_or(DEFAULT_FAST_INITIAL_WINDOW_S),
                ),
                // Floor at 1 s so a zero cannot spin the reconcile during boot.
                fast_initial_tick: Duration::from_secs(
                    r.fast_initial_tick_interval_s
                        .unwrap_or(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S)
                        .max(1),
                ),
            },
            None => RegReconcilerConfig::default(),
        },
        Err(_) => RegReconcilerConfig::default(),
    }
}

/// The wanted regulatory domain + rendezvous channel, read from the same
/// `video.wfb` block the radio uses. The reconciler never invents a domain; it
/// reuses the operator's configured value (or the safe default when absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedReg {
    pub domain: String,
    pub channel: u8,
}

/// Parse the wanted GLOBAL regulatory domain + rendezvous channel out of a config
/// body.
///
/// The wanted GLOBAL domain (the one the reconciler keeps so the onboard WiFi is
/// never stranded under a foreign baked country) resolves from the operating-
/// region posture:
/// - `network.regulatory.mode == region` (with a region code) → the pinned region
///   (so the global pin follows the operator's jurisdiction).
/// - otherwise (unrestricted) → `network.reg_reconciler.domain` if set, else the
///   legacy `video.wfb.reg_domain`, else the safe default `US`. The reconciler
///   never forces the world default `00`; an empty / malformed value falls back to
///   `US`.
///
/// The rendezvous channel resolution is unchanged (`video.wfb.rendezvous_channel`
/// when pinned, else `video.wfb.channel`, default 149).
pub fn read_wanted_from(text: &str) -> WantedReg {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        video: Video,
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Video {
        #[serde(default)]
        wfb: Wfb,
    }
    #[derive(serde::Deserialize, Default)]
    struct Wfb {
        #[serde(default)]
        reg_domain: Option<String>,
        #[serde(default)]
        channel: Option<u8>,
        #[serde(default)]
        rendezvous_channel: Option<u8>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        regulatory: Option<Regulatory>,
        #[serde(default)]
        reg_reconciler: Option<Reconciler>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Regulatory {
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        region: Option<String>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Reconciler {
        // The operator-overridable global domain the reconciler keeps under the
        // unrestricted posture. Optional; absent falls through to the legacy keys.
        #[serde(default)]
        domain: Option<String>,
    }
    let raw = serde_norway::from_str::<Raw>(text).unwrap_or_default();

    // A normalised, non-empty uppercase domain from an Option<String>, or None.
    let norm = |d: Option<String>| -> Option<String> {
        d.map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
    };

    // Region mode (with a valid region) pins the global domain to that region.
    let region_pin = raw.network.regulatory.as_ref().and_then(|r| {
        let is_region = r
            .mode
            .as_deref()
            .map(|m| m.trim().eq_ignore_ascii_case("region"))
            .unwrap_or(false);
        if is_region {
            norm(r.region.clone()).filter(|d| is_forceable_domain(d))
        } else {
            None
        }
    });

    let domain = region_pin
        // Unrestricted: prefer the reconciler's own override, then the legacy
        // video.wfb.reg_domain, then the safe default — never the world default.
        .or_else(|| norm(raw.network.reg_reconciler.and_then(|r| r.domain)))
        .or_else(|| norm(raw.video.wfb.reg_domain))
        .filter(|d| is_forceable_domain(d))
        .unwrap_or_else(|| DEFAULT_REG_DOMAIN.to_string());

    let channel = raw
        .video
        .wfb
        .rendezvous_channel
        .or(raw.video.wfb.channel)
        .unwrap_or(DEFAULT_CHANNEL);
    WantedReg { domain, channel }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_reconciler::RegReconcilerConfig;

    // ----- reconciler config parsing -----

    #[test]
    fn absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(
            cfg.tick_interval,
            Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
        );
        // The fast-initial window is ON by default so a fresh boot converges fast.
        assert_eq!(
            cfg.fast_initial_window,
            Duration::from_secs(DEFAULT_FAST_INITIAL_WINDOW_S)
        );
        assert_eq!(
            cfg.fast_initial_tick,
            Duration::from_secs(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S)
        );
    }

    #[test]
    fn explicit_disable_is_honored() {
        let cfg = read_config_from("network:\n  reg_reconciler:\n    enabled: false\n");
        assert!(!cfg.enabled);
    }

    #[test]
    fn explicit_interval_parses_and_floors_at_one() {
        let cfg = read_config_from(
            "network:\n  reg_reconciler:\n    enabled: true\n    tick_interval_s: 15\n",
        );
        assert!(cfg.enabled);
        assert_eq!(cfg.tick_interval, Duration::from_secs(15));
        let zero = read_config_from("network:\n  reg_reconciler:\n    tick_interval_s: 0\n");
        assert_eq!(zero.tick_interval, Duration::from_secs(1));
    }

    #[test]
    fn fast_initial_fields_parse_and_floor_the_tick() {
        let cfg = read_config_from(
            "network:\n  reg_reconciler:\n    fast_initial_window_s: 90\n    fast_initial_tick_interval_s: 3\n",
        );
        assert_eq!(cfg.fast_initial_window, Duration::from_secs(90));
        assert_eq!(cfg.fast_initial_tick, Duration::from_secs(3));
        // The fast tick floors at 1 s so a zero cannot spin the reconcile.
        let floored =
            read_config_from("network:\n  reg_reconciler:\n    fast_initial_tick_interval_s: 0\n");
        assert_eq!(floored.fast_initial_tick, Duration::from_secs(1));
    }

    #[test]
    fn fast_initial_window_zero_disables_the_fast_path() {
        // A zero window is honored verbatim (no floor): it disables the fast
        // convergence so the reconcile uses the steady cadence from boot.
        let cfg = read_config_from("network:\n  reg_reconciler:\n    fast_initial_window_s: 0\n");
        assert_eq!(cfg.fast_initial_window, Duration::ZERO);
        // With the window off, even uptime 0 yields the steady interval.
        assert_eq!(cfg.effective_interval(Duration::ZERO), cfg.tick_interval);
    }

    #[test]
    fn effective_interval_is_fast_inside_the_window_then_steady() {
        let cfg = RegReconcilerConfig::default();
        // Inside the window (uptime < 60 s): the fast cadence.
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(0)),
            cfg.fast_initial_tick
        );
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(59)),
            cfg.fast_initial_tick
        );
        // At/after the window boundary: the steady cadence (proven steady-state).
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(60)),
            cfg.tick_interval
        );
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(600)),
            cfg.tick_interval
        );
    }

    #[test]
    fn malformed_config_defaults_enabled() {
        let cfg = read_config_from(": : : not yaml");
        assert!(cfg.enabled);
        assert_eq!(
            cfg.tick_interval,
            Duration::from_secs(DEFAULT_TICK_INTERVAL_S)
        );
        assert_eq!(
            cfg.fast_initial_window,
            Duration::from_secs(DEFAULT_FAST_INITIAL_WINDOW_S)
        );
    }

    // ----- wanted domain + channel resolution (shared with the radio config) -----

    #[test]
    fn wanted_defaults_when_absent() {
        let w = read_wanted_from("agent:\n  name: x\n");
        assert_eq!(w.domain, "US");
        assert_eq!(w.channel, 149);
    }

    #[test]
    fn wanted_reads_reg_domain_and_channel() {
        let w = read_wanted_from("video:\n  wfb:\n    reg_domain: in\n    channel: 165\n");
        // Uppercased.
        assert_eq!(w.domain, "IN");
        assert_eq!(w.channel, 165);
    }

    #[test]
    fn wanted_rendezvous_pin_overrides_home_channel() {
        let w = read_wanted_from(
            "video:\n  wfb:\n    channel: 149\n    rendezvous_channel: 153\n    reg_domain: US\n",
        );
        assert_eq!(w.channel, 153);
        assert_eq!(w.domain, "US");
    }

    #[test]
    fn wanted_empty_reg_domain_falls_back_to_default() {
        let w = read_wanted_from("video:\n  wfb:\n    reg_domain: ''\n    channel: 149\n");
        assert_eq!(w.domain, "US");
    }

    #[test]
    fn wanted_region_pin_drives_the_global_domain() {
        // A pinned operating region makes the global wanted domain follow it, so
        // the onboard-WiFi global pin tracks the operator's jurisdiction.
        let w = read_wanted_from(
            "network:\n  regulatory:\n    mode: region\n    region: de\n\nvideo:\n  wfb:\n    channel: 149\n",
        );
        assert_eq!(w.domain, "DE");
        assert_eq!(w.channel, 149);
    }

    #[test]
    fn wanted_unrestricted_uses_reconciler_override_then_legacy_then_default() {
        // Unrestricted + an explicit reconciler domain override wins.
        let w = read_wanted_from(
            "network:\n  regulatory:\n    mode: unrestricted\n  reg_reconciler:\n    domain: gb\n",
        );
        assert_eq!(w.domain, "GB");
        // Unrestricted + no override → the legacy video.wfb.reg_domain.
        let w = read_wanted_from(
            "network:\n  regulatory:\n    mode: unrestricted\n\nvideo:\n  wfb:\n    reg_domain: us\n",
        );
        assert_eq!(w.domain, "US");
        // Unrestricted + nothing set anywhere → the safe default US.
        let w = read_wanted_from("network:\n  regulatory:\n    mode: unrestricted\n");
        assert_eq!(w.domain, "US");
    }

    #[test]
    fn wanted_region_pin_without_code_falls_back_to_unrestricted_resolution() {
        // A region mode with no code is not a forceable pin; the resolution falls
        // through to the unrestricted path (legacy reg_domain / default), never a
        // malformed global domain.
        let w = read_wanted_from(
            "network:\n  regulatory:\n    mode: region\n\nvideo:\n  wfb:\n    reg_domain: in\n",
        );
        assert_eq!(w.domain, "IN");
    }

    #[test]
    fn wanted_never_forces_world_default_from_a_region_pin() {
        // A `region: '00'` pin is not forceable (the reconciler never forces the
        // world default), so it falls through rather than capping the radio.
        let w = read_wanted_from(
            "network:\n  regulatory:\n    mode: region\n    region: '00'\n\nvideo:\n  wfb:\n    reg_domain: us\n",
        );
        assert_eq!(w.domain, "US");
    }
}
