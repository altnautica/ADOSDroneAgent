//! What kind of machine this install is on, and the desktop probes that decide
//! it.
//!
//! A ground station used to assume a dedicated box: it took the console,
//! masked sleep, pinned the Wi-Fi, forced the USB port into host mode and stood
//! the login manager down. On somebody's laptop every one of those is a
//! destructive act on a machine they are using. So the install classifies the
//! host first, persists the answer in `/etc/ados/host.conf`
//! ([`ados_protocol::host_kind`]), and a personal host skips the whole appliance
//! half of the graph.
//!
//! Classification is deliberately conservative in one direction: only an
//! unambiguous personal computer becomes [`HostKind::Personal`]. A board the
//! probe cannot describe — an SBC running a desktop image, a headless server, a
//! container — keeps the appliance behaviour every shipped unit set was
//! validated against. The failure mode of guessing "personal" on a drone is a
//! ground station that does not bring its own radio up; the failure mode of
//! guessing "appliance" on a laptop is a masked sleep target and a Wi-Fi profile
//! taken over, which the next install can undo and which the operator has
//! `--host personal` to correct immediately.

use std::path::Path;

use ados_protocol::host_kind::HostKind;
use ados_term::exec;

/// `chassis_type` values that mean "portable or space-constrained machine":
/// Portable, Laptop, Notebook, Hand Held, Sub Notebook, Tablet, Convertible and
/// Detachable. A desktop tower reads 3, and a server 23; both are covered by the
/// architecture rule below, so neither is listed here.
const LAPTOP_CHASSIS: [&str; 8] = ["8", "9", "10", "11", "14", "30", "31", "32"];

/// The facts that decide the host kind. A plain struct so [`classify`] is a pure
/// function the tests can drive without touching the machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostProbe {
    /// Whether a desktop stack is present (see [`is_desktop`]).
    pub desktop: bool,
    /// The normalized architecture (`aarch64`, `x86_64`, …).
    pub arch: String,
    /// `/sys/class/dmi/id/chassis_type` is one of [`LAPTOP_CHASSIS`].
    pub laptop_chassis: bool,
    /// A real battery is present: a system battery, not a device-scoped one (a
    /// mouse, a UPS or a wireless controller also report `type: Battery`).
    pub battery: bool,
}

/// Classify a probed host. Pure.
///
/// Personal means: a desktop stack is installed, AND the machine is either an
/// x86_64 PC (a laptop or a desktop tower — a general-purpose computer either
/// way), a laptop chassis, or battery-powered. That keeps every aarch64 SBC on
/// the appliance path even when it boots a desktop image, which is exactly the
/// board class whose bring-up depends on the appliance steps.
pub fn classify(p: &HostProbe) -> HostKind {
    let computer = p.arch == "x86_64" || p.laptop_chassis || p.battery;
    if p.desktop && computer {
        HostKind::Personal
    } else {
        HostKind::Appliance
    }
}

/// The facts that decide whether a desktop stack is present. Kept as a plain
/// struct so [`is_desktop`] is a pure, unit-tested function separate from the
/// system probe.
#[derive(Debug, Clone, Copy, Default)]
pub struct DesktopProbe {
    /// A display manager (gdm/sddm/lightdm, aliased to `display-manager.service`)
    /// is active.
    pub dm_active: bool,
    /// The GNOME shell binary is installed.
    pub gnome_shell: bool,
    /// The KDE Plasma shell binary is installed.
    pub plasmashell: bool,
}

/// True when the box has a desktop stack (a running display manager OR an
/// installed GNOME/KDE shell). "Installed" — not "showing" — is the right gate:
/// PackageKit + GNOME Software churn the moment a desktop stack is on disk, even
/// with no one logged in, and a desktop image is what makes a machine somebody
/// else's computer rather than an appliance. Pure.
pub fn is_desktop(p: DesktopProbe) -> bool {
    p.dm_active || p.gnome_shell || p.plasmashell
}

/// Probe the live box for a desktop. Best-effort: any read failure reads as
/// "absent" so a headless node cleanly skips the desktop block.
pub fn probe_desktop() -> DesktopProbe {
    let dm_active = [
        "display-manager.service",
        "gdm.service",
        "gdm3.service",
        "sddm.service",
        "lightdm.service",
    ]
    .iter()
    .any(|u| exec::run_ok("systemctl", &["is-active", "--quiet", u]));
    DesktopProbe {
        dm_active,
        gnome_shell: binary_present("gnome-shell"),
        plasmashell: binary_present("plasmashell"),
    }
}

/// True when `name` resolves to an executable on PATH (a desktop shell binary
/// being installed is the broad desktop-present signal). Best-effort: probes the
/// shell's `command -v` (PATH-aware), falling back to the canonical bin dirs.
/// `name` is a fixed shell name here, so the `sh -c` interpolation is not a
/// user-input injection surface.
fn binary_present(name: &str) -> bool {
    exec::run_ok("sh", &["-c", &format!("command -v {name}")])
        || Path::new(&format!("/usr/bin/{name}")).exists()
        || Path::new(&format!("/bin/{name}")).exists()
}

/// A logind session type that means a compositor is up in that session. Pure.
pub fn is_graphical_session_type(session_type: &str) -> bool {
    matches!(session_type.trim(), "x11" | "wayland" | "mir")
}

/// The logind session type of THIS process, or `None` when it cannot be
/// determined. `XDG_SESSION_TYPE` is deliberately not used: sudo's `env_reset`
/// strips it, so under the install one-liner it is always absent and would make
/// the stop unreachable. `/proc/self/sessionid` carries the audit/logind session
/// id across fork, sudo and setsid, so it survives the way the installer is
/// actually launched. Best-effort; any failure reads as unknown.
pub fn own_session_type() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/self/sessionid").ok()?;
    let id = raw.trim();
    // 4294967295 is the kernel's "no audit session" sentinel.
    if id.is_empty() || id == "4294967295" {
        return None;
    }
    let shown = exec::run("loginctl", &["show-session", id, "-p", "Type", "--value"]);
    if !shown.success() {
        return None;
    }
    let t = shown.stdout.trim().to_string();
    if t.is_empty() {
        return None;
    }
    Some(t)
}

/// Read one sysfs attribute, trimmed, or `None` when absent.
fn read_attr(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    let trimmed = body.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Whether this machine's DMI chassis type is a portable one.
fn laptop_chassis() -> bool {
    read_attr(Path::new("/sys/class/dmi/id/chassis_type"))
        .is_some_and(|t| LAPTOP_CHASSIS.contains(&t.as_str()))
}

/// Whether a real system battery is present.
///
/// `/sys/class/power_supply/*/type` reads `Battery` for a UPS, a wireless mouse
/// and a game controller too. Those carry a `scope` of `Device`; a system
/// battery has no `scope` file at all on the kernels we ship to, or `System`.
fn system_battery() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let dir = entry.path();
        if read_attr(&dir.join("type")).as_deref() != Some("Battery") {
            return false;
        }
        match read_attr(&dir.join("scope")) {
            None => true,
            Some(scope) => scope != "Device",
        }
    })
}

/// Probe the live box. Best-effort throughout: a read failure reads as "no",
/// which lands on the appliance side of [`classify`].
pub fn probe() -> HostProbe {
    HostProbe {
        desktop: is_desktop(probe_desktop()),
        arch: crate::env::arch().to_string(),
        laptop_chassis: laptop_chassis(),
        battery: system_battery(),
    }
}

/// The human-readable basis for a probe's verdict, for the closing summary and
/// the status line. Names only the signals that are actually present.
pub fn detection_basis(p: &HostProbe) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if p.desktop {
        parts.push("desktop session");
    }
    if p.laptop_chassis {
        parts.push("laptop chassis");
    }
    if p.battery {
        parts.push("battery");
    }
    if p.arch == "x86_64" {
        parts.push("x86_64");
    }
    if parts.is_empty() {
        "no desktop detected".to_string()
    } else {
        parts.join(", ")
    }
}

/// Resolve the host for this run: the `--host` flag wins, then the persisted
/// `/etc/ados/host.conf`, then a live probe. Returns the verdict and, when it
/// came from a probe rather than an explicit choice, the probe for reporting.
pub fn resolve(flag: Option<&str>) -> (HostKind, Option<HostProbe>) {
    if let Some(kind) = flag.and_then(HostKind::parse) {
        return (kind, None);
    }
    let conf = Path::new(ados_protocol::host_kind::HOST_CONF);
    // A persisted file is a decision already made (by an operator, or by a
    // previous install's probe); re-probing would let a machine that booted
    // into a different session flip its own unit set underneath itself.
    if conf.exists() {
        return (ados_protocol::host_kind::read_host_kind_from(conf), None);
    }
    let probe = probe();
    (classify(&probe), Some(probe))
}

/// The outcome a provisioner returns on a personal computer, or `None` when it
/// should run.
///
/// Every appliance-only provisioner begins with this: on somebody's own machine
/// the agent does not mask sleep, retune the Wi-Fi radio, rewrite the routing
/// table, force an I2C overlay or arm a hardware watchdog. The step is recorded
/// as skipped (not failed, not done) so the summary shows it was considered and
/// consciously left alone, and a log line names which one.
pub fn personal_skip(ctx: &crate::ctx::Ctx, step: &str) -> Option<crate::graph::StepOutcome> {
    if !ctx.host.is_personal() {
        return None;
    }
    tracing::info!(step, "skipped on a personal computer");
    Some(crate::graph::StepOutcome::Skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(desktop: bool, arch: &str, chassis: bool, battery: bool) -> HostProbe {
        HostProbe {
            desktop,
            arch: arch.to_string(),
            laptop_chassis: chassis,
            battery,
        }
    }

    #[test]
    fn an_x86_desktop_computer_is_personal() {
        assert_eq!(
            classify(&probe(true, "x86_64", false, false)),
            HostKind::Personal
        );
    }

    #[test]
    fn an_aarch64_board_with_a_desktop_image_stays_an_appliance() {
        // The board class whose bring-up depends on the appliance steps: a
        // desktop image on an SBC must not switch them off.
        assert_eq!(
            classify(&probe(true, "aarch64", false, false)),
            HostKind::Appliance
        );
    }

    #[test]
    fn a_headless_x86_machine_is_an_appliance() {
        assert_eq!(
            classify(&probe(false, "x86_64", false, false)),
            HostKind::Appliance
        );
        assert_eq!(classify(&HostProbe::default()), HostKind::Appliance);
    }

    #[test]
    fn an_aarch64_laptop_with_a_battery_is_personal() {
        assert_eq!(
            classify(&probe(true, "aarch64", false, true)),
            HostKind::Personal
        );
        assert_eq!(
            classify(&probe(true, "aarch64", true, false)),
            HostKind::Personal
        );
    }

    #[test]
    fn a_battery_alone_does_not_make_a_desktop() {
        // A headless battery-powered board is still an appliance.
        assert_eq!(
            classify(&probe(false, "aarch64", true, true)),
            HostKind::Appliance
        );
    }

    #[test]
    fn desktop_gate_is_true_for_any_desktop_signal() {
        assert!(!is_desktop(DesktopProbe::default()));
        assert!(is_desktop(DesktopProbe {
            dm_active: true,
            ..Default::default()
        }));
        assert!(is_desktop(DesktopProbe {
            gnome_shell: true,
            ..Default::default()
        }));
        assert!(is_desktop(DesktopProbe {
            plasmashell: true,
            ..Default::default()
        }));
    }

    #[test]
    fn graphical_session_types_are_the_compositor_ones() {
        for t in ["x11", "wayland", "mir"] {
            assert!(is_graphical_session_type(t), "{t} is a graphical session");
        }
        for t in ["tty", "unspecified", ""] {
            assert!(!is_graphical_session_type(t), "{t} is not graphical");
        }
    }

    #[test]
    fn the_chassis_list_is_the_portable_one() {
        assert!(LAPTOP_CHASSIS.contains(&"10"), "Notebook");
        assert!(!LAPTOP_CHASSIS.contains(&"3"), "Desktop");
        assert!(!LAPTOP_CHASSIS.contains(&"23"), "Rack Mount Chassis");
    }

    #[test]
    fn the_basis_names_only_present_signals() {
        assert_eq!(
            detection_basis(&probe(true, "x86_64", true, true)),
            "desktop session, laptop chassis, battery, x86_64"
        );
        assert_eq!(
            detection_basis(&probe(false, "aarch64", false, false)),
            "no desktop detected"
        );
    }

    #[test]
    fn an_explicit_flag_beats_probing() {
        assert_eq!(resolve(Some("personal")), (HostKind::Personal, None));
        assert_eq!(resolve(Some("appliance")), (HostKind::Appliance, None));
    }
}
