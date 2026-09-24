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
//! Independent of any grant, the agent's run directory ([`HIDDEN_RUN_DIR`]) is
//! replaced by an empty read-only tmpfs in the plugin's mount namespace, so the
//! agent's command sockets and the plugin host's control dir are simply not
//! there. Their real gate is the socket group (`ados-operator`, which the
//! plugin user is never in) plus a peer-credential check on accept; this is the
//! second line. Hiding the directory rather than listing each socket keeps it
//! closed when a service re-creates its socket after the plugin started: a
//! mount over a socket file stays on the old inode and the new socket would be
//! in plain view. Only [`PLUGIN_REACHABLE_SOCKETS`] are bound back in, and the
//! main unit binds the plugin's own socket directory (see
//! [`crate::systemd::render_unit`]), never another plugin's.
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
/// A `char-<name>` entry is a systemd device-node *group* name (the
/// `/proc/devices` name), so the rule covers every minor the kernel enumerates
/// — `char-i2c` matches `/dev/i2c-0` through `/dev/i2c-N` without the renderer
/// having to know how many buses a board exposes. An entry naming a `/dev`
/// path matches that one node; it is used where a driver registers as a misc
/// device or under a name that differs between driver releases.
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
    // render node for the buffer allocator. The DRM driver registers major 226
    // as `drm`.
    (
        "hardware.camera.csi",
        &["char-video4linux rw", "char-drm rw"],
    ),
    // GPU and neural accelerators: DRM card/render nodes (which also carry a
    // DRM-based RKNPU), the Mali kbase and legacy RKNPU misc nodes, and the
    // NVIDIA control, device, UVM and modeset nodes.
    (
        GPU_CAP,
        &[
            "char-drm rw",
            "/dev/mali0 rw",
            "/dev/rknpu rw",
            "/dev/nvidiactl rw",
            "/dev/nvidia0 rw",
            "/dev/nvidia-uvm rw",
            "/dev/nvidia-uvm-tools rw",
            "/dev/nvidia-modeset rw",
            "char-nvidia-frontend rw",
            "char-nvidia-uvm rw",
        ],
    ),
];

/// The capability that unlocks the GPU and neural-accelerator device nodes.
pub const GPU_CAP: &str = "hardware.gpu";

/// The groups the accelerator nodes are owned by on a stock Debian/Ubuntu
/// image; the plugin user joins them with the GPU grant, since the device
/// policy admits a node but the file mode still has to.
pub const GPU_SUPPLEMENTARY_GROUPS: &str = "video render";

/// The capability that lets a declared service bind its declared TCP ports.
pub const NETWORK_LISTEN_CAP: &str = "network.listen";

/// The capability that unlocks outbound sockets.
pub const NETWORK_OUTBOUND_CAP: &str = "network.outbound";

/// The capability that unlocks the host filesystem outside the plugin's tree.
pub const FILESYSTEM_HOST_CAP: &str = "filesystem.host";

/// Paths a plugin never reaches, granted or not: the HMAC issuer secret it
/// could mint another plugin's token from, and the trusted-key store it could
/// enrol its own signer into. File modes already keep `ados` out of both; this
/// is the second line, and it costs one line of unit text.
pub const ALWAYS_INACCESSIBLE: &[&str] = &["/etc/ados/secrets", "/etc/ados/plugin-keys"];

/// The agent's run directory, hidden from every plugin behind an empty
/// read-only tmpfs. It holds every agent command socket (the control plane,
/// the flight-controller byte lanes, the radio / video / GPIO / input command
/// sockets), the plugin host's control dir and every plugin's socket directory.
/// A plugin reaches what it is granted through its own host socket, which gates
/// every call on its token.
pub const HIDDEN_RUN_DIR: &str = "/run/ados";

/// Sockets under [`HIDDEN_RUN_DIR`] bound back into every plugin, read-only:
/// the log ingest sink, which only accepts log frames. Prefixed `-` in the unit
/// so a host without it is not a unit-start failure. A socket file bind stays
/// on the inode present at unit start, so after the log store re-creates its
/// socket the plugin's log shipping resumes at its next start; its own log
/// file is unaffected.
pub const PLUGIN_REACHABLE_SOCKETS: &[&str] = &["/run/ados/logd.sock"];

/// Operator data roots reachable only with `filesystem.host`. Every entry is
/// prefixed `-` in the rendered directive so a host that does not have the path
/// is not a unit-start failure.
pub const HOST_DATA_ROOTS: &[&str] = &["/srv", "/mnt", "/media", "/boot"];

/// The writable surface every plugin gets: its own data dir and its log.
/// Ordered, so the rendered `ReadWritePaths=` line is stable.
pub const BASE_READ_WRITE_PATHS: &[&str] = &["/var/ados/plugin-data", "/var/log/ados/plugins"];

/// Every capability whose enforcement mechanism is the generated unit.
///
/// Re-exported as [`crate::SANDBOX_ENFORCED_CAPS`]; the guard tests in
/// `lib.rs` union this with the wire-dispatch and handler gates to decide
/// whether the catalog's `enforced` flag is honest.
pub fn sandbox_enforced_caps() -> BTreeSet<&'static str> {
    let mut set: BTreeSet<&'static str> = DEVICE_CAP_RULES.iter().map(|(cap, _)| *cap).collect();
    set.insert(NETWORK_OUTBOUND_CAP);
    set.insert(NETWORK_LISTEN_CAP);
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
/// no-grant socket policy. `listen_ports` are the TCP ports the unit's process
/// declares it serves (a declared service's `listen_ports`; empty for the main
/// unit, which never listens): with `network.listen` granted each one gets a
/// `SocketBindAllow=`, and every unit denies every other bind.
pub fn sandbox_directives(
    granted: &BTreeSet<String>,
    loopback_guard_active: bool,
    listen_ports: &[u16],
) -> Vec<String> {
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
        if granted.contains(GPU_CAP) {
            lines.push(format!("SupplementaryGroups={GPU_SUPPLEMENTARY_GROUPS}"));
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
    // No plugin process binds an inet port unless it is a declared service
    // granted `network.listen`, and then only the ports it declared. A listener
    // also needs the inet families above, which only `network.outbound` opens.
    if granted.contains(NETWORK_LISTEN_CAP) {
        for port in listen_ports {
            lines.push(format!("SocketBindAllow=tcp:{port}"));
        }
    }
    lines.push("SocketBindDeny=any".to_string());

    // ---- filesystem -------------------------------------------------
    // The agent run dir goes first: an empty read-only tmpfs, with only the
    // plugin-reachable sockets bound back in.
    lines.push(format!("TemporaryFileSystem={HIDDEN_RUN_DIR}:ro"));
    for socket in PLUGIN_REACHABLE_SOCKETS {
        lines.push(format!("BindReadOnlyPaths=-{socket}"));
    }
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
        let lines = sandbox_directives(&caps(&[]), true, &[]);
        assert!(lines.contains(&"PrivateDevices=yes".to_string()));
        assert!(lines.contains(&"RestrictAddressFamilies=AF_UNIX".to_string()));
        assert!(lines.contains(&"IPAddressDeny=any".to_string()));
        assert!(!lines.iter().any(|l| l.starts_with("DeviceAllow=")));
    }

    #[test]
    fn granting_i2c_does_not_also_open_the_camera() {
        let lines = sandbox_directives(&caps(&["hardware.i2c"]), true, &[]);
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
        let lines = sandbox_directives(&caps(&["network.outbound"]), true, &[]);
        assert!(lines
            .contains(&"RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK".to_string()));
        // No systemd address filter: it would also cut ingress to the plugin's
        // own listener. The nftables guard closes the agent ports instead.
        assert!(!lines.iter().any(|l| l.starts_with("IPAddressDeny=")));
        assert!(!lines.iter().any(|l| l.starts_with("IPAddressAllow=")));
    }

    #[test]
    fn network_grant_without_the_loopback_guard_keeps_the_no_grant_socket_policy() {
        let lines = sandbox_directives(&caps(&["network.outbound"]), false, &[]);
        assert!(lines.contains(&"RestrictAddressFamilies=AF_UNIX".to_string()));
        assert!(lines.contains(&"IPAddressDeny=any".to_string()));
        assert_eq!(lines, sandbox_directives(&caps(&[]), false, &[]));
    }

    #[test]
    fn filesystem_grant_moves_the_data_roots_from_inaccessible_to_writable() {
        let without = sandbox_directives(&caps(&[]), true, &[]);
        let with = sandbox_directives(&caps(&["filesystem.host"]), true, &[]);
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

    /// The no-grant filesystem lines, restated as the literals the Python
    /// renderer's test also pins, so the two renderers cannot drift.
    const NO_GRANT_FILESYSTEM: &[&str] = &[
        "TemporaryFileSystem=/run/ados:ro",
        "BindReadOnlyPaths=-/run/ados/logd.sock",
        "ReadWritePaths=/var/ados/plugin-data /var/log/ados/plugins",
        "ProtectHome=yes",
        "InaccessiblePaths=-/etc/ados/secrets -/etc/ados/plugin-keys -/srv -/mnt -/media -/boot",
    ];

    #[test]
    fn the_agent_run_dir_is_hidden_whatever_is_granted() {
        let none = sandbox_directives(&caps(&[]), true, &[]);
        let tail = &none[none.len() - NO_GRANT_FILESYSTEM.len()..];
        assert_eq!(tail, NO_GRANT_FILESYSTEM);

        // Granting every sandbox capability reopens only the data roots: the
        // run dir stays an empty tmpfs, nothing under it becomes writable, and
        // nothing but the log sink is bound back, least of all another
        // plugin's socket dir.
        let everything: Vec<&str> = sandbox_enforced_caps().into_iter().collect();
        for lines in [
            none.clone(),
            sandbox_directives(&caps(&everything), true, &[]),
        ] {
            assert!(lines.contains(&"TemporaryFileSystem=/run/ados:ro".to_string()));
            for line in &lines {
                if let Some(paths) = line.strip_prefix("ReadWritePaths=") {
                    assert!(!paths.contains("/run/ados"), "{line}");
                }
                if line.starts_with("BindReadOnlyPaths=") || line.starts_with("BindPaths=") {
                    assert_eq!(line, "BindReadOnlyPaths=-/run/ados/logd.sock");
                }
            }
        }
    }

    #[test]
    fn uvc_and_csi_together_emit_one_video4linux_rule() {
        let lines = sandbox_directives(
            &caps(&["hardware.usb.uvc", "hardware.camera.csi"]),
            true,
            &[],
        );
        let count = lines
            .iter()
            .filter(|l| l.as_str() == "DeviceAllow=char-video4linux rw")
            .count();
        assert_eq!(count, 1, "{lines:?}");
        assert!(lines.contains(&"DeviceAllow=char-drm rw".to_string()));
    }

    #[test]
    fn the_same_grant_set_renders_identically() {
        let a = sandbox_directives(&caps(&["hardware.i2c", "network.outbound"]), true, &[]);
        let b = sandbox_directives(&caps(&["network.outbound", "hardware.i2c"]), true, &[]);
        assert_eq!(a, b);
    }

    #[test]
    fn only_a_granted_listener_may_bind_and_only_its_declared_ports() {
        // Every unit denies every inet bind.
        let none = sandbox_directives(&caps(&[]), true, &[8092]);
        assert!(none.contains(&"SocketBindDeny=any".to_string()));
        assert!(!none.iter().any(|l| l.starts_with("SocketBindAllow=")));
        // Declared ports without the grant stay closed.
        let declared_only = sandbox_directives(&caps(&["network.outbound"]), true, &[8092]);
        assert!(!declared_only
            .iter()
            .any(|l| l.starts_with("SocketBindAllow=")));
        // The grant opens exactly the declared ports and keeps the deny.
        let granted = sandbox_directives(
            &caps(&["network.outbound", "network.listen"]),
            true,
            &[8092, 8093],
        );
        let allows: Vec<&str> = granted
            .iter()
            .filter(|l| l.starts_with("SocketBindAllow="))
            .map(String::as_str)
            .collect();
        assert_eq!(
            allows,
            ["SocketBindAllow=tcp:8092", "SocketBindAllow=tcp:8093"]
        );
        assert!(granted.contains(&"SocketBindDeny=any".to_string()));
        // A unit with nothing to listen on gains nothing from the grant.
        assert_eq!(
            sandbox_directives(&caps(&["network.listen"]), true, &[]),
            sandbox_directives(&caps(&[]), true, &[])
        );
    }

    #[test]
    fn the_gpu_grant_opens_the_accelerator_nodes_and_their_groups() {
        let lines = sandbox_directives(&caps(&["hardware.gpu"]), true, &[]);
        assert!(lines.contains(&"DevicePolicy=closed".to_string()));
        for rule in [
            "DeviceAllow=char-drm rw",
            "DeviceAllow=/dev/mali0 rw",
            "DeviceAllow=/dev/nvidiactl rw",
        ] {
            assert!(
                lines.contains(&rule.to_string()),
                "{rule} missing: {lines:?}"
            );
        }
        assert!(lines.contains(&"SupplementaryGroups=video render".to_string()));
        // A CSI grant shares the DRM node but not the accelerator groups.
        let csi = sandbox_directives(&caps(&["hardware.camera.csi"]), true, &[]);
        assert!(!csi.iter().any(|l| l.starts_with("SupplementaryGroups=")));
        assert!(!csi
            .iter()
            .any(|l| l.contains("nvidia") || l.contains("mali")));
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
