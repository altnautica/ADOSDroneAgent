//! Receive-chain subprocess arguments, ports, lifecycle-state strings, and the
//! shared rx-key path.
//!
//! The GS arg sets differ from the drone-side `ados_radio::process` builders
//! (the ports are mirrored between rigs, so they are NOT reused verbatim), so
//! they live here as the receive plane's own builders. Pure — unit-tested
//! without spawning anything.

use std::path::{Path, PathBuf};

/// The safe default regulatory domain applied before monitor-mode bring-up when
/// the config carries none. Matches the air side's `WfbConfig` default and the
/// Python `DEFAULT_REG_DOMAIN`: U-NII-3 / channel 149 is permitted at usable TX
/// power, so the home rendezvous channel is not capped to the kernel's startup
/// domain (the -100 dBm "not permitted" sentinel).
pub const DEFAULT_REG_DOMAIN: &str = "US";

/// Internal data-RX egress port (the fan-out reads here). Differs from the
/// drone side's 5601 stats port.
pub const DATA_RX_PORT: u16 = 5599;
/// GS rx-control egress (decoded HopAnnounce/Presence → the listener's port).
pub const RX_CONTROL_PORT: u16 = 5803;
/// GS tx-control loopback ingress (HopAck/Presence out over the air).
pub const TX_CONTROL_PORT: u16 = 5810;
/// GS Atlas-aux egress: the drone radiates small Atlas events on radio_id 2 (the
/// aux application stream); the GS decodes them to this loopback port, where the
/// Atlas relay reads and re-POSTs them onto the LAN.
pub const ATLAS_RX_PORT: u16 = 5604;
/// wfb stats poll interval: the zombie watchdog cadence.
pub const RX_HEALTH_POLL_INTERVAL_S: f64 = 5.0;

/// The receive plane's top-level lifecycle string for the sidecar `state`
/// field. The drone side writes a sibling top-level `state`; the GS heartbeat
/// reads the sidecar raw, so without this key the GS link block reports a null
/// state. "active" once the data RX is up; "searching" while it is not.
pub const STATE_ACTIVE: &str = "active";
pub const STATE_SEARCHING: &str = "searching";
/// The receive plane refuses to bring up monitor mode / spawn the receive chain
/// until the wanted regulatory domain verifies and the rendezvous channel is
/// permitted. Mirrors the drone-side `reg_blocked` state so the panel shows the
/// regulatory conflict on either rig in one glance.
pub const STATE_REG_BLOCKED: &str = "reg_blocked";

/// Data-plane RX `wfb_rx` args for the ground profile. `-l 1000` enables the
/// per-second stats lines on stdout (without it the monitor stays empty and the
/// link reports disabled). Egress to the internal fan-out port 5599.
pub fn data_rx_args(iface: &str, rx_key: &Path, channel_port: u16) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        channel_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// GS Atlas-aux RX `wfb_rx` args: radio_id 2 (the aux application stream the
/// drone radiates small Atlas events on), decoded to `atlas_port`. Mirrors
/// `data_rx_args` with the aux radio_id; the asymmetric-by-direction aux pair
/// means the GS receives on `-p 2` (the drone egresses on p2), never p3.
pub fn gs_atlas_rx_args(iface: &str, rx_key: &Path, atlas_port: u16) -> Vec<String> {
    vec![
        "-p".into(),
        "2".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        atlas_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// GS rx-control `wfb_rx` args: radio_id 1, decode to the listener's port 5803.
pub fn gs_rx_control_args(iface: &str, rx_key: &Path) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        RX_CONTROL_PORT.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// GS tx-control `wfb_tx` args: radio_id 1, loopback ingress 5810, light FEC.
pub fn gs_tx_control_args(iface: &str, rx_key: &Path, mcs_index: u8) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-u".into(),
        TX_CONTROL_PORT.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-k".into(),
        "1".into(),
        "-n".into(),
        "2".into(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        mcs_index.to_string(),
        iface.into(),
    ]
}

/// Resolve the rx key path used by every receive subprocess. The data RX, both
/// control planes, and the stats decode all use the same `rx.key` (wfb-ng key
/// files carry both crypto_box halves so one file authenticates frames in both
/// directions).
pub(super) fn rx_key_path() -> PathBuf {
    PathBuf::from(ados_radio::paths::WFB_RX_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_radio::config::WfbConfig;

    #[test]
    fn data_rx_args_match_python() {
        // wfb_rx -p 0 -c 127.0.0.1 -u 5599 -K <rx.key> -l 1000 <iface>
        let a = data_rx_args("wlan1", Path::new("/etc/ados/wfb/rx.key"), DATA_RX_PORT);
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-c",
                "127.0.0.1",
                "-u",
                "5599",
                "-K",
                "/etc/ados/wfb/rx.key",
                "-l",
                "1000",
                "wlan1"
            ]
        );
    }

    #[test]
    fn gs_atlas_rx_uses_radio_id_2_and_the_atlas_port() {
        // The drone egresses Atlas events on the aux radio_id 2; the GS receives
        // on p2 (NOT p3), decoding to ATLAS_RX_PORT.
        let a = gs_atlas_rx_args("wlan1", Path::new("/etc/ados/wfb/rx.key"), ATLAS_RX_PORT);
        assert_eq!(a[0], "-p");
        assert_eq!(a[1], "2");
        let u = a.iter().position(|x| x == "-u").unwrap();
        assert_eq!(a[u + 1], "5604");
        assert_eq!(a.last().unwrap(), "wlan1");
    }

    #[test]
    fn gs_rx_control_uses_5803_not_drone_side_5810() {
        // The GS rx-control egress is 5803 (the listener's port), the mirror of
        // the drone side's 5810. This is the asymmetry the task flags.
        let a = gs_rx_control_args("wlan1", Path::new("/k"));
        let u = a.iter().position(|x| x == "-u").unwrap();
        assert_eq!(a[u + 1], "5803");
        assert_eq!(a[1], "1"); // radio_id 1
    }

    #[test]
    fn gs_tx_control_uses_5810_and_light_fec() {
        let a = gs_tx_control_args("wlan1", Path::new("/k"), 3);
        let u = a.iter().position(|x| x == "-u").unwrap();
        assert_eq!(a[u + 1], "5810");
        let k = a.iter().position(|x| x == "-k").unwrap();
        assert_eq!(a[k + 1], "1"); // light FEC k=1
        let m = a.iter().position(|x| x == "-M").unwrap();
        assert_eq!(a[m + 1], "3"); // mcs passed through
    }

    #[test]
    fn default_reg_domain_matches_air_side() {
        // The GS default regulatory domain must equal the air side's so both
        // rigs enable the same channel set (the home channel 149 is permitted).
        assert_eq!(DEFAULT_REG_DOMAIN, "US");
        assert_eq!(DEFAULT_REG_DOMAIN, WfbConfig::default().reg_domain.unwrap());
    }
}
