//! The capability-to-sandbox map: how a granted permission changes the
//! generated systemd unit.
//!
//! Most plugin capabilities are enforced on the wire, because the plugin has to
//! ask the host for the thing. A handful cannot be: opening `/dev/i2c-1`,
//! calling `socket(AF_INET)`, or reading `/srv` are direct syscalls inside the
//! plugin's own process, with no RPC the host could gate. For those the grant
//! has to change the *sandbox*, and the only place that can express it is the
//! unit file systemd starts the plugin from.
//!
//! So this module is the whole enforcement story for those capabilities:
//!
//! * **device capabilities** (`hardware.uart`, `hardware.i2c`, `hardware.spi`,
//!   `hardware.gpio`, `hardware.usb`, `hardware.usb.uvc`,
//!   `hardware.camera.csi`) map to cgroup device-controller rules. With none of
//!   them granted the unit gets `PrivateDevices=yes`, which replaces `/dev`
//!   with a private instance holding only the pseudo-devices and implies
//!   `DevicePolicy=closed`. With one or more granted the unit gets
//!   `DevicePolicy=closed` plus exactly the `DeviceAllow=` lines those grants
//!   name, so a plugin approved for I2C cannot open the camera.
//! * **`network.outbound`** maps to the socket policy. Ungranted, the unit gets
//!   `RestrictAddressFamilies=AF_UNIX` (the plugin still needs its own IPC
//!   socket) plus `IPAddressDeny=any`; `socket(AF_INET)` then fails with
//!   `EAFNOSUPPORT` inside the plugin. Granted, `AF_INET`/`AF_INET6`/
//!   `AF_NETLINK` are added and the address filter is lifted; the agent's own
//!   loopback listeners, where a loopback peer is trusted as on-box, stay
//!   closed through the nftables rule in [`crate::loopback_guard`], which
//!   matches the plugin user and the agent ports only (a systemd address
//!   filter would also cut the host's probe of the plugin's own listener).
//!   When that guard is not loaded, a granted unit keeps the no-grant socket
//!   policy: the capability is not safe to hold without it.
//! * **`filesystem.host`** maps to the mount namespace. Ungranted, the operator
//!   data roots are `InaccessiblePaths`; granted, they become writable and
//!   `ProtectHome` relaxes to read-only.
//!
//! Independent of any grant, the agent's own command sockets are
//! `InaccessiblePaths` too ([`AGENT_SOCKET_PATHS`]). Their real gate is the
//! socket group (`ados-operator`, which the plugin user is never in) plus a
//! peer-credential check on accept; hiding them from the plugin's mount
//! namespace is the second line.
//!
//! Two invariants this file exists to hold:
//!
//! 1. The map is **byte-identical** to `ados.plugins.systemd` on the Python
//!    side, because either lifecycle path may be the one that rendered a given
//!    unit (the Python supervisor owns the local REST path; this crate owns the
//!    cloud-relay path). `tests/test_plugins_systemd_sandbox.py` asserts the two
//!    renderers agree.
//! 2. Every capability named here appears in
//!    [`SANDBOX_ENFORCED_CAPS`](crate::SANDBOX_ENFORCED_CAPS), which the
//!    capability-catalog guard tests read to decide whether an `enforced = true`
//!    row is telling the truth.
//!
//! **A grant only takes effect when the unit is re-rendered.** The supervisors
//! rewrite the unit and restart the plugin on every grant and revoke; without
//! that the sandbox would still hold the set approved at install time, which is
//! the security-control-that-reports-itself-applied failure this map replaced.

use std::collections::BTreeSet;

/// One device capability and the cgroup device rules it unlocks.
///
/// The right-hand side is a systemd device-node *group* name (the
/// `/proc/devices` name, matched by `DeviceAllow=char-<name>`), not a path, so
/// the rule covers every minor the kernel enumerates — `char-i2c` matches
/// `/dev/i2c-0` through `/dev/i2c-N` without the renderer having to know how
/// many buses a board exposes.
pub const DEVICE_CAP_RULES: &[(&str, &[&str])] = &[
    // A UART plugin may be handed a USB serial adapter, a native UART, or a CDC
    // ACM modem; all three are the same grant to an operator.
    (
        "hardware.uart",
        &["char-ttyUSB rw", "char-ttyACM rw", "char-tty rw"],
    ),
    ("hardware.i2c", &["char-i2c rw"]),
    ("hardware.spi", &["char-spidev rw"]),
    ("hardware.gpio", &["char-gpiochip rw"]),
    // Raw bulk transfers go through the usbfs character devices under
    // /dev/bus/usb.
    ("hardware.usb", &["char-usb_device rw"]),
    ("hardware.usb.uvc", &["char-video4linux rw"]),
    // A CSI capture path is V4L2 plus, on the Rockchip and Broadcom ISPs, a DRM
    // render node for the buffer allocator.
    (
        "hardware.camera.csi",
        &["char-video4linux rw", "char-dri rw"],
    ),
];

/// The capability that unlocks outbound sockets.
pub const NETWORK_OUTBOUND_CAP: &str = "network.outbound";

/// The capability that unlocks the host filesystem outside the plugin's tree.
pub const FILESYSTEM_HOST_CAP: &str = "filesystem.host";

/// Paths a plugin never reaches, granted or not: the HMAC issuer secret it
/// could mint another plugin's token from, and the trusted-key store it could
/// enrol its own signer into. File modes already keep `ados` out of both; this
/// is the second line, and it costs one line of unit text.
pub const ALWAYS_INACCESSIBLE: &[&str] = &["/etc/ados/secrets", "/etc/ados/plugin-keys"];

/// Agent command sockets, and the plugin host's control dir, hidden from every
/// plugin. Each is a command surface that acts with the agent's authority
/// rather than the plugin's grants (the control plane, the flight-controller
/// byte lanes, the radio / video / GPIO / input command sockets). A plugin
/// reaches the ones it is granted through its own host socket, which gates
/// every call on its token. Prefixed `-` like the other entries, so a socket a
/// host does not run is not a unit-start failure.
pub const AGENT_SOCKET_PATHS: &[&str] = &[
    "/run/ados/plugin-host",
    "/run/ados/control.sock",
    "/run/ados/api-internal.sock",
    "/run/ados/mavlink.sock",
    "/run/ados/msp.sock",
    "/run/ados/supervisor.sock",
    "/run/ados/radio-cmd.sock",
    "/run/ados/radio-aux.sock",
    "/run/ados/wfb-cmd.sock",
    "/run/ados/video-cmd.sock",
    "/run/ados/gpio-cmd.sock",
    "/run/ados/hid-cmd.sock",
    "/run/ados/pic.sock",
    "/run/ados/crsf-cmd.sock",
    "/run/ados/wifi-cmd.sock",
    "/run/ados/groundlink-cmd.sock",
    "/run/ados/tunnel-config-cmd.sock",
    "/run/ados/atlas-control.sock",
    "/run/ados/pairing.sock",
    "/run/ados/logd-query.sock",
];

/// Operator data roots reachable only with `filesystem.host`. Every entry is
/// prefixed `-` in the rendered directive so a host that does not have the path
/// is not a unit-start failure.
pub const HOST_DATA_ROOTS: &[&str] = &["/srv", "/mnt", "/media", "/boot"];

/// The writable surface every plugin gets: its own data dir, its log, and its
/// socket dir. Ordered, so the rendered `ReadWritePaths=` line is stable.
pub const BASE_READ_WRITE_PATHS: &[&str] = &[
    "/var/ados/plugin-data",
    "/var/log/ados/plugins",
    "/run/ados/plugins",
];

/// Every capability whose enforcement mechanism is the generated unit.
///
/// Re-exported as [`crate::SANDBOX_ENFORCED_CAPS`]; the guard tests in
/// `lib.rs` union this with the wire-dispatch and handler gates to decide
/// whether the catalog's `enforced` flag is honest.
pub fn sandbox_enforced_caps() -> BTreeSet<&'static str> {
    let mut set: BTreeSet<&'static str> = DEVICE_CAP_RULES.iter().map(|(cap, _)| *cap).collect();
    set.insert(NETWORK_OUTBOUND_CAP);
    set.insert(FILESYSTEM_HOST_CAP);
    set
}

/// The `[Service]` lines that express `granted` as a sandbox, in the order they
/// are rendered into the unit.
///
/// Deterministic for a given grant set: the device rules follow
/// [`DEVICE_CAP_RULES`] order and the path lists follow their constants, so
/// re-rendering an unchanged grant set produces a byte-identical unit and the
/// supervisors can skip the restart. `loopback_guard_active` is the verdict of
/// [`crate::loopback_guard`]; without it a `network.outbound` grant renders the
/// no-grant socket policy.
pub fn sandbox_directives(granted: &BTreeSet<String>, loopback_guard_active: bool) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    // ---- devices ----------------------------------------------------
    let device_rules: Vec<&str> = DEVICE_CAP_RULES
        .iter()
        .filter(|(cap, _)| granted.contains(*cap))
        .flat_map(|(_, rules)| rules.iter().copied())
        .collect();
    if device_rules.is_empty() {
        // The strictest posture systemd offers: a private /dev with only the
        // pseudo-devices, which also implies DevicePolicy=closed.
        lines.push("PrivateDevices=yes".to_string());
    } else {
        lines.push("DevicePolicy=closed".to_string());
        // Dedupe while keeping rule order: two granted caps can name the same
        // device group (uvc and csi both want char-video4linux) and systemd
        // would take the duplicate, but a stable unit text is worth more.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for rule in device_rules {
            if seen.insert(rule) {
                lines.push(format!("DeviceAllow={rule}"));
            }
        }
    }

    // ---- sockets ----------------------------------------------------
    if granted.contains(NETWORK_OUTBOUND_CAP) && loopback_guard_active {
        // AF_NETLINK rides with the grant because a plugin that may reach the
        // network needs getifaddrs / DNS resolution to do it. The agent's own
        // loopback listeners stay closed through the nftables guard.
        lines.push("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK".to_string());
    } else {
        // AF_UNIX stays: the plugin's own host socket is a Unix socket, so
        // denying it would deny the plugin everything.
        lines.push("RestrictAddressFamilies=AF_UNIX".to_string());
        lines.push("IPAddressDeny=any".to_string());
    }

    // ---- filesystem -------------------------------------------------
    let host_fs = granted.contains(FILESYSTEM_HOST_CAP);
    let mut rw: Vec<String> = BASE_READ_WRITE_PATHS
        .iter()
        .map(|p| p.to_string())
        .collect();
    if host_fs {
        rw.extend(HOST_DATA_ROOTS.iter().map(|p| p.to_string()));
    }
    lines.push(format!("ReadWritePaths={}", rw.join(" ")));
    lines.push(
        if host_fs {
            "ProtectHome=read-only"
        } else {
            "ProtectHome=yes"
        }
        .to_string(),
    );
    let mut inaccessible: Vec<String> = ALWAYS_INACCESSIBLE
        .iter()
        .chain(AGENT_SOCKET_PATHS)
        .map(|p| format!("-{p}"))
        .collect();
    if !host_fs {
        inaccessible.extend(HOST_DATA_ROOTS.iter().map(|p| format!("-{p}")));
    }
    lines.push(format!("InaccessiblePaths={}", inaccessible.join(" ")));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_grant_gives_private_dev_and_no_outbound_sockets() {
        let lines = sandbox_directives(&caps(&[]), true);
        assert!(lines.contains(&"PrivateDevices=yes".to_string()));
        assert!(lines.contains(&"RestrictAddressFamilies=AF_UNIX".to_string()));
        assert!(lines.contains(&"IPAddressDeny=any".to_string()));
        assert!(!lines.iter().any(|l| l.starts_with("DeviceAllow=")));
    }

    #[test]
    fn granting_i2c_does_not_also_open_the_camera() {
        let lines = sandbox_directives(&caps(&["hardware.i2c"]), true);
        assert!(lines.contains(&"DevicePolicy=closed".to_string()));
        assert!(lines.contains(&"DeviceAllow=char-i2c rw".to_string()));
        assert!(!lines
            .iter()
            .any(|l| l.contains("char-video4linux") || l.contains("char-spidev")));
        // PrivateDevices would hide the very node the grant unlocked.
        assert!(!lines.contains(&"PrivateDevices=yes".to_string()));
    }

    #[test]
    fn network_grant_opens_the_inet_families_when_the_loopback_guard_is_active() {
        let lines = sandbox_directives(&caps(&["network.outbound"]), true);
        assert!(lines
            .contains(&"RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK".to_string()));
        // No systemd address filter: it would also cut ingress to the plugin's
        // own listener. The nftables guard closes the agent ports instead.
        assert!(!lines.iter().any(|l| l.starts_with("IPAddressDeny=")));
        assert!(!lines.iter().any(|l| l.starts_with("IPAddressAllow=")));
    }

    #[test]
    fn network_grant_without_the_loopback_guard_keeps_the_no_grant_socket_policy() {
        let lines = sandbox_directives(&caps(&["network.outbound"]), false);
        assert!(lines.contains(&"RestrictAddressFamilies=AF_UNIX".to_string()));
        assert!(lines.contains(&"IPAddressDeny=any".to_string()));
        assert_eq!(lines, sandbox_directives(&caps(&[]), false));
    }

    #[test]
    fn filesystem_grant_moves_the_data_roots_from_inaccessible_to_writable() {
        let without = sandbox_directives(&caps(&[]), true);
        let with = sandbox_directives(&caps(&["filesystem.host"]), true);
        let rw_without = without
            .iter()
            .find(|l| l.starts_with("ReadWritePaths="))
            .unwrap();
        let rw_with = with
            .iter()
            .find(|l| l.starts_with("ReadWritePaths="))
            .unwrap();
        assert!(!rw_without.contains("/mnt"));
        assert!(rw_with.contains("/mnt"));
        let inacc_without = without
            .iter()
            .find(|l| l.starts_with("InaccessiblePaths="))
            .unwrap();
        let inacc_with = with
            .iter()
            .find(|l| l.starts_with("InaccessiblePaths="))
            .unwrap();
        assert!(inacc_without.contains("-/mnt"));
        assert!(!inacc_with.contains("-/mnt"));
        // The issuer secret is off limits in both postures.
        assert!(inacc_without.contains("-/etc/ados/secrets"));
        assert!(inacc_with.contains("-/etc/ados/secrets"));
    }

    /// The no-grant `InaccessiblePaths=` line, restated as the literal the
    /// Python renderer's test also pins, so the two renderers cannot drift.
    const NO_GRANT_INACCESSIBLE: &str = "InaccessiblePaths=-/etc/ados/secrets \
        -/etc/ados/plugin-keys -/run/ados/plugin-host -/run/ados/control.sock \
        -/run/ados/api-internal.sock -/run/ados/mavlink.sock -/run/ados/msp.sock \
        -/run/ados/supervisor.sock -/run/ados/radio-cmd.sock -/run/ados/radio-aux.sock \
        -/run/ados/wfb-cmd.sock -/run/ados/video-cmd.sock -/run/ados/gpio-cmd.sock \
        -/run/ados/hid-cmd.sock -/run/ados/pic.sock -/run/ados/crsf-cmd.sock \
        -/run/ados/wifi-cmd.sock -/run/ados/groundlink-cmd.sock \
        -/run/ados/tunnel-config-cmd.sock -/run/ados/atlas-control.sock \
        -/run/ados/pairing.sock -/run/ados/logd-query.sock -/srv -/mnt -/media -/boot";

    #[test]
    fn agent_command_sockets_are_hidden_whatever_is_granted() {
        let inaccessible = |granted: &BTreeSet<String>| {
            sandbox_directives(granted, true)
                .into_iter()
                .find(|l| l.starts_with("InaccessiblePaths="))
                .unwrap()
        };
        assert_eq!(inaccessible(&caps(&[])), NO_GRANT_INACCESSIBLE);

        // Granting every sandbox capability reopens only the data roots.
        let everything: Vec<&str> = sandbox_enforced_caps().into_iter().collect();
        let all = inaccessible(&caps(&everything));
        for path in AGENT_SOCKET_PATHS {
            assert!(
                all.contains(&format!(" -{path}")),
                "{path} missing from {all}"
            );
        }
        // The per-plugin socket dir stays reachable: the plugin's own host
        // socket lives there.
        assert!(!all.contains("/run/ados/plugins"), "{all}");
    }

    #[test]
    fn uvc_and_csi_together_emit_one_video4linux_rule() {
        let lines = sandbox_directives(&caps(&["hardware.usb.uvc", "hardware.camera.csi"]), true);
        let count = lines
            .iter()
            .filter(|l| l.as_str() == "DeviceAllow=char-video4linux rw")
            .count();
        assert_eq!(count, 1, "{lines:?}");
        assert!(lines.contains(&"DeviceAllow=char-dri rw".to_string()));
    }

    #[test]
    fn the_same_grant_set_renders_identically() {
        let a = sandbox_directives(&caps(&["hardware.i2c", "network.outbound"]), true);
        let b = sandbox_directives(&caps(&["network.outbound", "hardware.i2c"]), true);
        assert_eq!(a, b);
    }

    #[test]
    fn every_device_rule_cap_is_a_real_capability() {
        for cap in sandbox_enforced_caps() {
            assert!(
                ados_protocol::capabilities::get_agent_capability(cap).is_some(),
                "{cap} is sandbox-enforced but is not a declared agent capability"
            );
        }
    }
}
