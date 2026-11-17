//! Network-backend abstraction + the per-rung command matrix.
//!
//! The agent runs management over either NetworkManager or systemd-networkd
//! (with a raw `ip`/`dhclient` fallback). The per-rung command matrix is pure
//! (`rung_command`) so the whole matrix is unit-tested on every host; the
//! backend probe and the command runner are Linux-only.

use super::detection::Transport;
use super::ladder::RepairRung;

/// The network management backend in use on the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    NetworkManager,
    Networkd,
    /// Neither managed stack is active: raw `ip` / `dhclient`.
    Fallback,
}

impl Backend {
    /// The bland string for this backend (event detail + sidecar).
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::NetworkManager => "networkmanager",
            Backend::Networkd => "networkd",
            Backend::Fallback => "fallback",
        }
    }
}

/// Build the ordered argv list for one repair rung on one backend. Each inner
/// vector is one command run in sequence. An empty result means "no command for
/// this rung on this backend" (the reg-reassert rung is handled by the caller,
/// not as a shell command). Link-dropping rungs are atomic `sh -c "down; up"`
/// so the local daemon completes the restore even if the operator's link
/// blips. Pure.
pub fn rung_command(
    backend: Backend,
    rung: RepairRung,
    iface: &str,
    transport: Transport,
) -> Vec<Vec<String>> {
    let argv = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<String>>();
    let sh = |script: String| vec!["sh".to_string(), "-c".to_string(), script];
    match (backend, rung) {
        // The reg-reassert rung is a global, channel-safety-gated `iw reg set`
        // the caller runs via the shared regulatory reconcile, not a command here.
        (_, RepairRung::ReassertReg) | (_, RepairRung::Exhausted) => Vec::new(),

        (Backend::NetworkManager, RepairRung::RenewDhcp) => {
            vec![argv(&["nmcli", "device", "reapply", iface])]
        }
        (Backend::Networkd, RepairRung::RenewDhcp) => {
            vec![argv(&["networkctl", "renew", iface])]
        }
        (Backend::Fallback, RepairRung::RenewDhcp) => {
            vec![argv(&["dhclient", "-r", iface]), argv(&["dhclient", iface])]
        }

        (Backend::NetworkManager, RepairRung::ReconnectWifi) => {
            vec![argv(&["nmcli", "device", "reconnect", iface])]
        }
        (_, RepairRung::ReconnectWifi) => {
            vec![argv(&["wpa_cli", "-i", iface, "reconnect"])]
        }

        (Backend::NetworkManager, RepairRung::BounceIface) => vec![sh(format!(
            "nmcli device disconnect {iface} ; nmcli device connect {iface}"
        ))],
        (Backend::Networkd, RepairRung::BounceIface) => vec![sh(format!(
            "ip link set {iface} down && ip link set {iface} up && networkctl reconfigure {iface}"
        ))],
        (Backend::Fallback, RepairRung::BounceIface) => vec![sh(format!(
            "ip link set {iface} down && ip link set {iface} up"
        ))],

        (Backend::NetworkManager, RepairRung::RestartBackend) => {
            vec![argv(&["systemctl", "restart", "NetworkManager"])]
        }
        (Backend::Networkd, RepairRung::RestartBackend) => {
            let mut cmds = vec![argv(&["systemctl", "restart", "systemd-networkd"])];
            if transport == Transport::Wifi {
                cmds.push(argv(&[
                    "systemctl",
                    "restart",
                    &format!("wpa_supplicant@{iface}"),
                ]));
            }
            cmds
        }
        (Backend::Fallback, RepairRung::RestartBackend) => {
            vec![argv(&["systemctl", "restart", "systemd-networkd"])]
        }
    }
}

/// Whether NetworkManager manages `iface`, from `nmcli -t -f DEVICE,STATE
/// device status` output (pure). A device it lists as `unmanaged` (a wired
/// link another stack runs, kept out of NetworkManager by a conf.d rule) or
/// does not list at all is not its to repair.
pub fn nm_manages(device_status: &str, iface: &str) -> bool {
    device_status.lines().any(|line| {
        let mut cols = line.splitn(2, ':');
        cols.next() == Some(iface) && cols.next().is_some_and(|s| s.trim() != "unmanaged")
    })
}

/// Detect the backend that runs `iface` by querying systemd and
/// NetworkManager. NetworkManager wins only for a device it manages; a host can
/// run it for the Wi-Fi while systemd-networkd keeps the wired link. A box
/// with neither managed stack active (e.g. a CI host with no running systemd)
/// reports `Fallback`, which the guardian treats as "do not run the disruptive
/// ladder".
#[cfg(target_os = "linux")]
pub async fn detect_backend(iface: &str) -> Backend {
    if super::run_status("systemctl", &["is-active", "--quiet", "NetworkManager"]).await {
        // nmcli not answering keeps the old verdict: NetworkManager runs it.
        match super::run_output("nmcli", &["-t", "-f", "DEVICE,STATE", "device", "status"]).await {
            Some(status) if !nm_manages(&status, iface) => {}
            _ => return Backend::NetworkManager,
        }
    }
    if super::run_status("systemctl", &["is-active", "--quiet", "systemd-networkd"]).await {
        return Backend::Networkd;
    }
    Backend::Fallback
}

/// Run one repair rung's command(s) in sequence, best-effort. Returns true when
/// every command exited zero. The reg-reassert and exhausted rungs have no
/// command here (the caller handles reg-reassert) and return true.
#[cfg(target_os = "linux")]
pub async fn run_rung(
    backend: Backend,
    rung: RepairRung,
    iface: &str,
    transport: Transport,
) -> bool {
    let mut all_ok = true;
    for cmd in rung_command(backend, rung, iface, transport) {
        if cmd.is_empty() {
            continue;
        }
        let prog = cmd[0].as_str();
        let args: Vec<&str> = cmd[1..].iter().map(|s| s.as_str()).collect();
        if !super::run_status(prog, &args).await {
            all_ok = false;
        }
    }
    all_ok
}

#[cfg(test)]
mod tests;
