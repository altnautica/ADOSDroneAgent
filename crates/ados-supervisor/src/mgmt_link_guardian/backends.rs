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

/// Detect the active management backend by querying systemd. A box with neither
/// managed stack active (e.g. a CI host with no running systemd) reports
/// `Fallback`, which the guardian treats as "do not run the disruptive ladder".
#[cfg(target_os = "linux")]
pub async fn detect_backend() -> Backend {
    if super::run_status("systemctl", &["is-active", "--quiet", "NetworkManager"]).await {
        return Backend::NetworkManager;
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
mod tests {
    use super::*;

    #[test]
    fn reassert_and_exhausted_have_no_command() {
        for b in [
            Backend::NetworkManager,
            Backend::Networkd,
            Backend::Fallback,
        ] {
            assert!(
                rung_command(b, RepairRung::ReassertReg, "eth0", Transport::Ethernet).is_empty()
            );
            assert!(rung_command(b, RepairRung::Exhausted, "eth0", Transport::Ethernet).is_empty());
        }
    }

    #[test]
    fn renew_dhcp_matrix() {
        assert_eq!(
            rung_command(
                Backend::NetworkManager,
                RepairRung::RenewDhcp,
                "wlan0",
                Transport::Wifi
            ),
            vec![vec![
                "nmcli".to_string(),
                "device".to_string(),
                "reapply".to_string(),
                "wlan0".to_string()
            ]]
        );
        assert_eq!(
            rung_command(
                Backend::Networkd,
                RepairRung::RenewDhcp,
                "eth0",
                Transport::Ethernet
            ),
            vec![vec![
                "networkctl".to_string(),
                "renew".to_string(),
                "eth0".to_string()
            ]]
        );
        // Fallback issues a release then a fresh request.
        assert_eq!(
            rung_command(
                Backend::Fallback,
                RepairRung::RenewDhcp,
                "eth0",
                Transport::Ethernet
            )
            .len(),
            2
        );
    }

    #[test]
    fn bounce_is_atomic_down_then_up() {
        let nd = rung_command(
            Backend::Networkd,
            RepairRung::BounceIface,
            "eth0",
            Transport::Ethernet,
        );
        assert_eq!(nd.len(), 1);
        assert_eq!(nd[0][0], "sh");
        assert_eq!(nd[0][1], "-c");
        // One atomic command: the down and the up are in the same shell so the
        // local daemon always completes the up.
        assert!(nd[0][2].contains("ip link set eth0 down && ip link set eth0 up"));
        assert!(nd[0][2].contains("networkctl reconfigure eth0"));

        let fb = rung_command(
            Backend::Fallback,
            RepairRung::BounceIface,
            "eth0",
            Transport::Ethernet,
        );
        assert!(fb[0][2].contains("ip link set eth0 down && ip link set eth0 up"));

        let nm = rung_command(
            Backend::NetworkManager,
            RepairRung::BounceIface,
            "wlan0",
            Transport::Wifi,
        );
        assert!(nm[0][2].contains("nmcli device disconnect wlan0"));
        assert!(nm[0][2].contains("nmcli device connect wlan0"));
    }

    #[test]
    fn reconnect_wifi_matrix() {
        assert_eq!(
            rung_command(
                Backend::NetworkManager,
                RepairRung::ReconnectWifi,
                "wlan0",
                Transport::Wifi
            )[0],
            vec![
                "nmcli".to_string(),
                "device".to_string(),
                "reconnect".to_string(),
                "wlan0".to_string()
            ]
        );
        assert_eq!(
            rung_command(
                Backend::Networkd,
                RepairRung::ReconnectWifi,
                "wlan0",
                Transport::Wifi
            )[0],
            vec![
                "wpa_cli".to_string(),
                "-i".to_string(),
                "wlan0".to_string(),
                "reconnect".to_string()
            ]
        );
    }

    #[test]
    fn restart_backend_matrix_adds_wpa_on_wifi_networkd() {
        assert_eq!(
            rung_command(
                Backend::NetworkManager,
                RepairRung::RestartBackend,
                "wlan0",
                Transport::Wifi
            ),
            vec![vec![
                "systemctl".to_string(),
                "restart".to_string(),
                "NetworkManager".to_string()
            ]]
        );
        // networkd + Wi-Fi also restarts the per-iface supplicant.
        let nd_wifi = rung_command(
            Backend::Networkd,
            RepairRung::RestartBackend,
            "wlan0",
            Transport::Wifi,
        );
        assert_eq!(nd_wifi.len(), 2);
        assert_eq!(nd_wifi[1][2], "wpa_supplicant@wlan0");
        // networkd + Ethernet does not.
        let nd_eth = rung_command(
            Backend::Networkd,
            RepairRung::RestartBackend,
            "eth0",
            Transport::Ethernet,
        );
        assert_eq!(nd_eth.len(), 1);
    }

    #[test]
    fn backend_strings_are_bland() {
        assert_eq!(Backend::NetworkManager.as_str(), "networkmanager");
        assert_eq!(Backend::Networkd.as_str(), "networkd");
        assert_eq!(Backend::Fallback.as_str(), "fallback");
    }
}
