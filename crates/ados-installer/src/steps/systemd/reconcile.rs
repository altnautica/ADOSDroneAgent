//! Reconcilers that mirror operator config onto the markers and unit state
//! the shipped units gate on. Each runs on every install, so a partial state
//! self-heals.

use super::*;

/// Parse `radio.crsf.enabled` from config.yaml text. Absent / malformed /
/// not-present → false (the RC lane is opt-in). Pure.
pub fn parse_crsf_enabled(text: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        radio: Radio,
    }
    #[derive(serde::Deserialize, Default)]
    struct Radio {
        #[serde(default)]
        crsf: Option<Crsf>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Crsf {
        #[serde(default)]
        enabled: bool,
    }
    serde_norway::from_str::<Raw>(text)
        .ok()
        .and_then(|r| r.radio.crsf)
        .map(|c| c.enabled)
        .unwrap_or(false)
}

/// Reconcile the CRSF RC-lane enable marker from the operator config: the
/// `/etc/ados/crsf-enabled` marker the ados-crsf unit gates on
/// (`ConditionPathExists`) mirrors `radio.crsf.enabled`, so enabling the lane
/// in config is sufficient — no manual systemctl. Idempotent; runs on every
/// install so a partial state self-heals. The runtime half (a config write
/// through the agent) reconciles the same marker and kicks the unit itself.
pub(super) fn reconcile_crsf_marker() {
    let enabled = std::fs::read_to_string(crate::env::CONFIG_YAML)
        .map(|text| parse_crsf_enabled(&text))
        .unwrap_or(false);
    let marker = Path::new(CONFIG_DIR).join("crsf-enabled");
    if enabled {
        if let Err(e) = std::fs::write(&marker, b"") {
            tracing::warn!(error = %e, "crsf enable marker write failed");
        }
    } else {
        let _ = std::fs::remove_file(&marker);
    }
}

/// Parse `radio.tunnel.enabled` from config.yaml text. Absent / malformed /
/// not-present → false (the config-over-radio channel is opt-in). Pure.
pub fn parse_tunnel_enabled(text: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        radio: Radio,
    }
    #[derive(serde::Deserialize, Default)]
    struct Radio {
        #[serde(default)]
        tunnel: Option<Tunnel>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Tunnel {
        #[serde(default)]
        enabled: bool,
    }
    serde_norway::from_str::<Raw>(text)
        .ok()
        .and_then(|r| r.radio.tunnel)
        .map(|t| t.enabled)
        .unwrap_or(false)
}

/// Parse `network.hotspot.enabled` from config.yaml text. Absent / malformed /
/// not-present → false (the ground-station setup AP is opt-in). Pure.
pub fn parse_hotspot_enabled(text: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        network: Network,
    }
    #[derive(serde::Deserialize, Default)]
    struct Network {
        #[serde(default)]
        hotspot: Option<Hotspot>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Hotspot {
        #[serde(default)]
        enabled: bool,
    }
    serde_norway::from_str::<Raw>(text)
        .ok()
        .and_then(|r| r.network.hotspot)
        .map(|h| h.enabled)
        .unwrap_or(false)
}

/// Reconcile the ground-station setup-AP enable marker from the operator
/// config: the `/etc/ados/hotspot-enabled` marker the ados-dnsmasq-gs unit
/// gates on (`ConditionPathExists`) mirrors `network.hotspot.enabled`.
///
/// Without it the DHCP/DNS unit starts whenever hostapd's unit is active — but
/// the hostapd service stays `active` even when the operator never opted in
/// (it idles in place rather than exiting, so the supervisor does not read a
/// clean exit as a crash). dnsmasq then binds the AP address on an interface
/// that is, on a box using its onboard radio as a WiFi client, already carrying
/// a lease — and fails on every restart, leaving a permanently failed unit on a
/// perfectly healthy ground station. Idempotent; runs on every install so a
/// partial state self-heals.
pub(super) fn reconcile_hotspot_marker() {
    const GATED_UNIT: &str = "ados-dnsmasq-gs.service";
    let enabled = std::fs::read_to_string(crate::env::CONFIG_YAML)
        .map(|text| parse_hotspot_enabled(&text))
        .unwrap_or(false);
    let marker = Path::new(CONFIG_DIR).join("hotspot-enabled");
    if enabled {
        if let Err(e) = std::fs::write(&marker, b"") {
            tracing::warn!(error = %e, "hotspot enable marker write failed");
        }
    } else {
        let _ = std::fs::remove_file(&marker);
        // Clear any latched failure before the gate takes effect. systemd
        // remembers a unit's last failed state until it is reset, so a box
        // upgrading from the version that started this unit unconditionally
        // would keep reporting a failed unit forever even though the
        // condition now skips it cleanly — exactly the false status surface
        // this gate exists to remove.
        let _ = exec::run("systemctl", &["reset-failed", GATED_UNIT]);
    }
}

/// Parse `hardware.gpio_out_enabled` from config.yaml text. Absent / malformed
/// → `true` (the native GPIO-output path is on by default: it is required for
/// the ground station's beeper/status output, and the marker merely un-gates it
/// from a fresh flash that ships the service exec'ing `/bin/true` until the
/// marker exists). An explicit `false` opts the native path out. Pure.
pub fn parse_gpio_rust_enabled(text: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        hardware: Hardware,
    }
    #[derive(serde::Deserialize, Default)]
    struct Hardware {
        #[serde(default)]
        gpio_out_enabled: Option<bool>,
    }
    serde_norway::from_str::<Raw>(text)
        .ok()
        .and_then(|r| r.hardware.gpio_out_enabled)
        .unwrap_or(true)
}

/// Reconcile the native GPIO-output enable marker from the operator config: the
/// `/etc/ados/gpio-rust-enabled` marker the `ados-gpio` unit gates on
/// (`ConditionPathExists`; without it the service execs `/bin/true`) mirrors
/// `hardware.gpio_out_enabled`. Default true, so a fresh flash with no key
/// present boots the real GPIO service — the beeper and status lines are part
/// of the ground station's shipped behaviour, not an opt-in. An explicit
/// `false` in config removes the marker and the unit stays inert. Idempotent;
/// runs on every install so a partial state self-heals.
pub(super) fn reconcile_gpio_rust_marker() {
    const GATED_UNIT: &str = "ados-gpio.service";
    let enabled = std::fs::read_to_string(crate::env::CONFIG_YAML)
        .map(|text| parse_gpio_rust_enabled(&text))
        .unwrap_or(true);
    let marker = Path::new(CONFIG_DIR).join("gpio-rust-enabled");
    if enabled {
        if let Err(e) = std::fs::write(&marker, b"") {
            tracing::warn!(error = %e, "gpio-rust enable marker write failed");
        }
        // Kick the gate only when the marker was newly created or rewritten on
        // an already-provisioned node; a fresh flash starts it via the marker
        // on first boot.
        let _ = exec::run("systemctl", &["kill", "-s", "SIGHUP", GATED_UNIT]);
    } else {
        let _ = std::fs::remove_file(&marker);
    }
}

/// Reconcile the config-over-radio enable marker from the operator config: the
/// `/etc/ados/tunnel-enabled` marker the ados-tunnel-config unit gates on
/// (`ConditionPathExists`) mirrors `radio.tunnel.enabled`, so enabling the
/// channel in config is sufficient — no manual systemctl. Idempotent; runs on
/// every install so a partial state self-heals. The runtime half (a config
/// write through the agent) reconciles the same marker and kicks the unit.
pub(super) fn reconcile_tunnel_marker() {
    let enabled = std::fs::read_to_string(crate::env::CONFIG_YAML)
        .map(|text| parse_tunnel_enabled(&text))
        .unwrap_or(false);
    let marker = Path::new(CONFIG_DIR).join("tunnel-enabled");
    if enabled {
        if let Err(e) = std::fs::write(&marker, b"") {
            tracing::warn!(error = %e, "tunnel enable marker write failed");
        }
    } else {
        let _ = std::fs::remove_file(&marker);
    }
}

/// Parse `lora.enabled` from config.yaml text. Absent / malformed /
/// not-present → false (the LoRa lane is opt-in). Pure.
pub fn parse_lora_enabled(text: &str) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        lora: Option<Lora>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Lora {
        #[serde(default)]
        enabled: bool,
    }
    serde_norway::from_str::<Raw>(text)
        .ok()
        .and_then(|r| r.lora)
        .map(|l| l.enabled)
        .unwrap_or(false)
}

/// Reconcile the `/etc/ados/lora-enabled` marker the ados-lora unit gates on
/// from `lora.enabled`. Idempotent; runs on every install.
pub(super) fn reconcile_lora_marker() {
    let enabled = std::fs::read_to_string(crate::env::CONFIG_YAML)
        .map(|text| parse_lora_enabled(&text))
        .unwrap_or(false);
    let marker = Path::new(CONFIG_DIR).join("lora-enabled");
    if enabled {
        if let Err(e) = std::fs::write(&marker, b"") {
            tracing::warn!(error = %e, "lora enable marker write failed");
        }
    } else {
        let _ = std::fs::remove_file(&marker);
    }
}

/// Decide whether the logging and telemetry store unit should be enabled, from
/// the config body. Pure, so the default and the override are asserted without
/// a filesystem or a systemd.
///
/// The store is **off unless asked for**. It is ~96% of everything the box
/// writes to its card, and the largest single lump of space it occupies, so a
/// node that nobody configured for it does not run it. `logging.store.enabled`
/// is the one key; there is no marker file that overrides it in either
/// direction, so "why is this on?" is always answered by the config alone.
pub fn logd_unit_wanted(config_body: Option<&str>) -> bool {
    config_body
        .map(ados_config::log_store::enabled_from)
        .unwrap_or(false)
}

/// Reconcile the local logging and telemetry store unit against the config key.
/// Idempotent and runs on every install so a re-run from a partial state
/// self-heals. The START half is the `start` step's job (the unit is PartOf the
/// supervisor).
///
/// Turning it back on is one config key plus this reconcile, never a reinstall
/// from scratch — the binary is still fetched and placed either way.
pub(super) fn reconcile_logd_unit() {
    const UNIT: &str = "ados-logd.service";
    let config_body = std::fs::read_to_string(Path::new(CONFIG_DIR).join("config.yaml")).ok();
    if logd_unit_wanted(config_body.as_deref()) {
        // Un-mask first: a box that carried the store while it was off may have
        // been masked by an earlier reconcile, and `enable` on a masked unit is
        // a silent no-op that would leave the operator's opt-in inert.
        let _ = exec::run("systemctl", &["unmask", UNIT]);
        enable_if_present(UNIT);
    } else {
        let _ = exec::run("systemctl", &["stop", UNIT]);
        let _ = exec::run("systemctl", &["disable", UNIT]);
        // Mask as well as disable: `disable` only drops the wants-symlinks, and
        // this unit is pulled in by the supervisor, so a dependency would start
        // it again. The daemon's own gate would decline, but the unit would
        // still churn a start attempt on every boot.
        let _ = exec::run("systemctl", &["mask", UNIT]);
        let _ = exec::run("systemctl", &["reset-failed", UNIT]);
    }
}

/// The systemd drop-in directory + body that bind the native control surface to
/// the LAN port as the front. Pure so the body is unit-testable.
pub(super) const FRONT_CONTROL_DROPIN_DIR: &str = "/etc/systemd/system/ados-control.service.d";
pub(super) const FRONT_API_DROPIN_DIR: &str = "/etc/systemd/system/ados-api.service.d";
pub(super) const FRONT_DROPIN_NAME: &str = "front.conf";
/// The residual API's internal socket path the front reverse-proxies to. Must
/// match the Rust proxy default + the Python `internal_socket_path` reader.
pub(super) const FRONT_API_INTERNAL_SOCKET: &str = "/run/ados/api-internal.sock";

/// The control drop-in body: bind the native surface to the LAN port the GCS
/// uses.
pub(super) fn front_control_dropin_body() -> &'static str {
    "[Service]\nEnvironment=ADOS_CONTROL_PORT=8080\n"
}

/// The api drop-in body: keep the residual API off the LAN port, on the
/// internal Unix socket the front proxies to.
pub(super) fn front_api_dropin_body() -> String {
    format!("[Service]\nEnvironment=ADOS_API_INTERNAL_SOCKET={FRONT_API_INTERNAL_SOCKET}\n")
}

/// Write the LAN-front drop-ins: the native control surface owns the LAN port
/// (:8080) and the residual API sits behind it on an internal Unix socket. The
/// front is unconditional — it is the only LAN surface that answers the
/// migrated routes. Runs its own `daemon-reload` (the flow's earlier reload
/// predates these files); the actual unit restart is the start step's
/// supervisor cycle. Idempotent; a partial state self-heals on the next install.
pub(super) fn reconcile_front_unit() {
    let control_dropin = Path::new(FRONT_CONTROL_DROPIN_DIR).join(FRONT_DROPIN_NAME);
    let api_dropin = Path::new(FRONT_API_DROPIN_DIR).join(FRONT_DROPIN_NAME);
    let _ = std::fs::create_dir_all(FRONT_CONTROL_DROPIN_DIR);
    let _ = write_atomic(&control_dropin, front_control_dropin_body());
    let _ = std::fs::create_dir_all(FRONT_API_DROPIN_DIR);
    let _ = write_atomic(&api_dropin, front_api_dropin_body());
    let _ = exec::run("systemctl", &["daemon-reload"]);
    tracing::info!("native control surface bound to the LAN port");
}
