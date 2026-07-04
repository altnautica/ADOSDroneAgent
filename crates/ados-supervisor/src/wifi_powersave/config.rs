//! Config for the WiFi power-save runtime reconciler.
//!
//! The FullMAC WiFi drivers (Broadcom `brcmfmac` and friends) bring the station
//! interface up with 802.11 power-save ENABLED, which parks the radio between
//! beacons. On an otherwise-idle onboard link that silently drops unicast frames
//! (broadcast ARP still lands, but a unicast SSH / ping times out) — the classic
//! "the box vanishes when it goes quiet" signature. Install- and boot-time
//! provisioning already sets power-save off, but the driver re-enables it after a
//! reconnect, a WiFi hotplug, or a driver reload. This module holds the cadence
//! config for the always-on runtime half, parsed from `network.wifi_powersave`.
//! Pure parsing — unit-tested on every host; the canonical-path read lives in
//! `os`.

use std::time::Duration;

/// Default steady reconcile cadence. Re-asserting power-save off is cheap (one
/// `iw get` per interface, a `set` only on a real drift), so a 30 s spacing keeps
/// a driver that silently re-enabled power-save from leaving the link idle-dead
/// for long without shelling `iw` every monitor pass.
pub(super) const DEFAULT_TICK_INTERVAL_S: u64 = 30;

/// Default duration after process start during which the reconcile runs at the
/// faster `fast_initial_tick` cadence. Boot and first-association is when the
/// driver is most likely to bring the link up with its default (power-save on),
/// so converging fast here closes the window before the operator first tries to
/// reach the box. Measured against process uptime so a supervisor restart re-arms
/// the fast window.
pub(super) const DEFAULT_FAST_INITIAL_WINDOW_S: u64 = 60;

/// Default reconcile cadence during the fast-initial window. Short enough that
/// power-save re-enabled by a fresh association is corrected within a few
/// seconds, but still a throttle (not a busy loop).
pub(super) const DEFAULT_FAST_INITIAL_TICK_INTERVAL_S: u64 = 5;

/// Configuration for the WiFi power-save reconciler, read from
/// `network.wifi_powersave`. Default-ON so a fresh board keeps a reliable onboard
/// link out of the box; an operator can disable it cleanly if a bespoke setup
/// ever needs power-save left as the driver set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiPowersaveConfig {
    /// Whether the reconciler runs at all. Default true.
    pub enabled: bool,
    /// Minimum spacing between reconcile attempts in steady state.
    pub tick_interval: Duration,
    /// How long after process start the reconcile runs at the faster
    /// `fast_initial_tick` cadence. Default 60 s. A zero disables the fast window
    /// (the reconcile uses the steady cadence from boot).
    pub fast_initial_window: Duration,
    /// The reconcile cadence during the fast-initial window. Default 5 s, floored
    /// at 1 s. Only used while uptime is below `fast_initial_window`.
    pub fast_initial_tick: Duration,
}

impl Default for WifiPowersaveConfig {
    fn default() -> Self {
        WifiPowersaveConfig {
            enabled: true,
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_S),
            fast_initial_window: Duration::from_secs(DEFAULT_FAST_INITIAL_WINDOW_S),
            fast_initial_tick: Duration::from_secs(DEFAULT_FAST_INITIAL_TICK_INTERVAL_S),
        }
    }
}

impl WifiPowersaveConfig {
    /// The effective reconcile cadence given the current process uptime. Inside
    /// the fast-initial window (and when that window is enabled) the faster
    /// cadence applies; after the window it settles to the steady cadence. Pure so
    /// the schedule is unit-tested without a clock.
    pub fn effective_interval(&self, uptime: Duration) -> Duration {
        if !self.fast_initial_window.is_zero() && uptime < self.fast_initial_window {
            self.fast_initial_tick
        } else {
            self.tick_interval
        }
    }
}

/// Parse `network.wifi_powersave` out of a config body. An absent section reads
/// as the all-defaults (enabled) config so the reconciler is on out of the box.
/// A malformed config also falls back to enabled rather than silently disabling
/// the onboard-link protection (the loud-fail loader logs the exact parse error).
pub fn read_config_from(text: &str) -> WifiPowersaveConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Net,
    }
    #[derive(serde::Deserialize, Default)]
    struct Net {
        #[serde(default)]
        wifi_powersave: Option<Recon>,
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
    let raw: Raw = ados_config::yaml_or_default(text, "wifi_powersave");
    match raw.network.wifi_powersave {
        Some(r) => WifiPowersaveConfig {
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
        None => WifiPowersaveConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cfg = read_config_from("network:\n  wifi_powersave:\n    enabled: false\n");
        assert!(!cfg.enabled);
    }

    #[test]
    fn explicit_interval_parses_and_floors_at_one() {
        let cfg = read_config_from(
            "network:\n  wifi_powersave:\n    enabled: true\n    tick_interval_s: 15\n",
        );
        assert!(cfg.enabled);
        assert_eq!(cfg.tick_interval, Duration::from_secs(15));
        let zero = read_config_from("network:\n  wifi_powersave:\n    tick_interval_s: 0\n");
        assert_eq!(zero.tick_interval, Duration::from_secs(1));
    }

    #[test]
    fn fast_initial_fields_parse_and_floor_the_tick() {
        let cfg = read_config_from(
            "network:\n  wifi_powersave:\n    fast_initial_window_s: 90\n    fast_initial_tick_interval_s: 3\n",
        );
        assert_eq!(cfg.fast_initial_window, Duration::from_secs(90));
        assert_eq!(cfg.fast_initial_tick, Duration::from_secs(3));
        // The fast tick floors at 1 s so a zero cannot spin the reconcile.
        let floored =
            read_config_from("network:\n  wifi_powersave:\n    fast_initial_tick_interval_s: 0\n");
        assert_eq!(floored.fast_initial_tick, Duration::from_secs(1));
    }

    #[test]
    fn fast_initial_window_zero_disables_the_fast_path() {
        // A zero window is honored verbatim (no floor): it disables the fast
        // convergence so the reconcile uses the steady cadence from boot.
        let cfg = read_config_from("network:\n  wifi_powersave:\n    fast_initial_window_s: 0\n");
        assert_eq!(cfg.fast_initial_window, Duration::ZERO);
        // With the window off, even uptime 0 yields the steady interval.
        assert_eq!(cfg.effective_interval(Duration::ZERO), cfg.tick_interval);
    }

    #[test]
    fn effective_interval_is_fast_inside_the_window_then_steady() {
        let cfg = WifiPowersaveConfig::default();
        // Inside the window (uptime < 60 s): the fast cadence.
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(0)),
            cfg.fast_initial_tick
        );
        assert_eq!(
            cfg.effective_interval(Duration::from_secs(59)),
            cfg.fast_initial_tick
        );
        // At/after the window boundary: the steady cadence.
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
}
