//! Argument builders for each wfb plane.

use super::*;

/// True when a Reed-Solomon `(k, n)` ratio is valid for `wfb_tx`: a positive
/// data-shard count and at least one parity shard (`n > k`).
pub fn fec_ratio_valid(fec_k: u8, fec_n: u8) -> bool {
    fec_k != 0 && fec_n > fec_k
}

/// True when an MCS index is in the accepted RTL8812EU VHT80 range (0..=7).
pub fn mcs_index_valid(mcs: u8) -> bool {
    mcs <= 7
}

/// Build the data-plane config for a whole-group respawn: the boot-time `cfg`
/// with the data-plane FEC/MCS trio overlaid from the live retained tunables
/// (`data_fec_k`/`data_fec_n`/`data_mcs_index`). Keeps every other field (iface,
/// band, channel, ports, power) from the boot config so only the data tier is
/// preserved across a hop — the operator's pinned manual tier or the adaptive
/// FEC/MCS is not reverted to the boot defaults. Pure so a respawn can assert the
/// retained trio reaches the data-plane args without spawning a real process.
pub fn data_cfg_from_retained(cfg: &WfbConfig, fec_k: u8, fec_n: u8, mcs_index: u8) -> WfbConfig {
    WfbConfig {
        fec_k,
        fec_n,
        mcs_index,
        ..cfg.clone()
    }
}

pub(super) fn key_str(key_path: &Path) -> String {
    key_path
        .to_str()
        .unwrap_or("/etc/ados/wfb/tx.key")
        .to_string()
}

/// Data-plane `wfb_tx` args (radio_id 0, UDP 5600, video FEC k/n from config).
/// `link_id` is this drone's own (`link_id(fleet_id, fleet_slot)`): the video
/// downlink is transmitted under the drone's identity so each drone in a fleet
/// occupies its own `channel_id` on the shared channel.
///
/// `-C` binds the wfb-ng 24.08 management socket ([`crate::tx_cmd`]) on
/// `8000 + radio_port`, so the adaptive controller can retune FEC and MCS on the
/// running transmitter instead of killing and respawning it (a 1-2 s video gap).
pub fn data_tx_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-i".into(),
        link_id.to_string(),
        "-C".into(),
        crate::tx_cmd::control_port(0).to_string(),
        "-u".into(),
        "5600".into(),
        "-K".into(),
        key_str(key_path),
        "-k".into(),
        cfg.fec_k.to_string(),
        "-n".into(),
        cfg.fec_n.to_string(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        cfg.mcs_index.to_string(),
        iface.into(),
    ]
}

/// Control-plane `wfb_tx` args (radio_id 1, UDP 5803, light FEC k=1/n=2).
/// `link_id` is this drone's own: HopAck / PresenceBeacon travel downlink, so
/// they are keyed to the drone that emitted them. `-C` binds this plane's own
/// management socket (see [`data_tx_args`]).
pub fn tx_control_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-i".into(),
        link_id.to_string(),
        "-C".into(),
        crate::tx_cmd::control_port(1).to_string(),
        "-u".into(),
        "5803".into(),
        "-K".into(),
        key_str(key_path),
        "-k".into(),
        "1".into(),
        "-n".into(),
        "2".into(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        cfg.mcs_index.to_string(),
        iface.into(),
    ]
}

/// Control-plane `wfb_rx` args (radio_id 1, re-emit HopAck on 127.0.0.1:5810).
/// `link_id` is the GROUND STATION's (`link_id(fleet_id, SLOT_GROUND)`), not
/// this drone's: the uplink is a single shared transmitter every drone in the
/// fleet listens to, so a fleet-wide command is one transmission, not N.
///
/// `-l 1000` makes this receiver emit the per-second `PKT`/`RX_ANT` stats lines
/// on stdout, and this is the ONLY link measurement a transmitting drone can
/// obtain: it is the one `channel_id` frames actually arrive on. A separate
/// stats receiver used to run alongside it on `-p 0` keyed to this drone's OWN
/// `link_id` — the drone's own video downlink — which a single radio in monitor
/// mode cannot capture, and with the wrong key besides, so it decoded nothing
/// for the process lifetime while `wfb-stats.json` reported the untouched
/// sentinels as a reading. Meanwhile this receiver's correctly-keyed stats
/// stream went to `/dev/null`. The caller now pipes and reads it.
pub fn rx_control_args(iface: &str, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-i".into(),
        link_id.to_string(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        "5810".into(),
        "-K".into(),
        key_str(key_path),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}

/// Auxiliary-stream `wfb_tx` args (radio_id 2). A separate radio-port from the
/// data plane (0) and control plane (1) carrying an application stream on its own
/// UDP ingress port, with its own (light) Reed-Solomon ratio and MCS. The aux
/// pair shares the injection adapter but never the radio_id, so it can never
/// collide with the data or control planes on the air.
///
/// `link_id` is this drone's own — the aux downlink is the drone talking. `-C`
/// binds this plane's own management socket (see [`data_tx_args`]).
pub fn aux_tx_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "2".into(),
        "-i".into(),
        link_id.to_string(),
        "-C".into(),
        crate::tx_cmd::control_port(2).to_string(),
        "-u".into(),
        cfg.aux_tx_port.to_string(),
        "-K".into(),
        key_str(key_path),
        "-k".into(),
        cfg.aux_fec_k.to_string(),
        "-n".into(),
        cfg.aux_fec_n.to_string(),
        "-B".into(),
        "20".into(),
        "-M".into(),
        cfg.aux_mcs().to_string(),
        iface.into(),
    ]
}

/// Auxiliary-stream `wfb_rx` args (radio_id 3, re-emit decoded application frames
/// on 127.0.0.1:`aux_rx_port`). The re-emit port is deliberately distinct from
/// the aux tx ingress (`aux_tx_port`) so the receive side can never feed back into
/// the transmit ingress. Uses the same key path as the aux tx.
///
/// `link_id` is the GROUND STATION's: like the control uplink, the aux uplink is
/// one shared transmitter the whole fleet receives.
pub fn aux_rx_args(iface: &str, cfg: &WfbConfig, key_path: &Path, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "3".into(),
        "-i".into(),
        link_id.to_string(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        cfg.aux_rx_port.to_string(),
        "-K".into(),
        key_str(key_path),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
}
