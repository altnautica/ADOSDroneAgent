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

/// Per-slot loopback egress port for a drone's video (`wfb_rx -p 0`). The
/// ground station runs one receiver per registered fleet slot, so each needs
/// its own egress: N H.264 streams cannot share one UDP port — they would
/// interleave into an undecodable mux. The fan-out reads the primary slot's
/// port here.
pub const fn video_rx_port(slot: u8) -> u16 {
    ados_radio::config::VIDEO_RX_PORT_BASE + slot as u16
}

/// Per-slot loopback egress port for a drone's aux application lane
/// (`wfb_rx -p 2`): MAVLink, relayed status, and RPC responses. One aux
/// consumer binds each of these.
pub const fn aux_rx_port(slot: u8) -> u16 {
    ados_radio::config::AUX_RX_PORT_BASE + slot as u16
}

/// Per-slot loopback egress port for a drone's control lane (`wfb_rx -p 1`):
/// decoded HopAnnounce / PresenceBeacon frames. The presence listener binds one
/// of these per registered slot.
pub const fn control_rx_port(slot: u8) -> u16 {
    ados_radio::config::CONTROL_RX_PORT_BASE + slot as u16
}

/// GS tx-control loopback ingress (HopAck/Presence out over the air). NOT
/// per-slot: the ground station runs exactly one control transmitter keyed to
/// its own slot 0, which every drone in the fleet receives, so a fleet-wide
/// control frame is one transmission rather than N.
pub const TX_CONTROL_PORT: u16 = 5810;
/// GS aux-uplink loopback ingress: whatever is written here is radiated on radio_id 3,
/// the ground→drone half of the aux pair. It matches the drone's `aux_tx_port` default
/// (the two rigs never share a host, so the same number on both sides is the symmetric
/// choice, not a collision). NOT per-slot, for the same reason as [`TX_CONTROL_PORT`]:
/// one uplink transmitter serves the whole fleet. Deliberately outside the per-slot
/// receive span so the uplink ingress can never be fed by a downlink egress.
pub const AUX_TX_PORT: u16 = 5602;
// Kept as the DEFAULT only. `gs_aux_tx_args` takes the resolved value, because
// the port is operator-settable and the writers into this lane resolve it from
// config; a constant here is correct at exactly one setting.
/// wfb stats poll interval: the zombie watchdog cadence.
pub const RX_HEALTH_POLL_INTERVAL_S: f64 = 5.0;

/// The receive plane's top-level lifecycle string for the sidecar `state`
/// field. The drone side writes a sibling top-level `state`; the GS heartbeat
/// reads the sidecar raw, so without this key the GS link block reports a null
/// state. "active" once the data RX is up; "searching" while it is not.
pub const STATE_ACTIVE: &str = "active";
pub const STATE_SEARCHING: &str = "searching";
/// The receive plane refuses to bring up monitor mode / spawn the receive chain until
/// the wanted regulatory domain verifies and the rendezvous channel is permitted.
pub const STATE_REG_BLOCKED: &str = "reg_blocked";
/// A receive adapter was selected but injection setup did not establish (the
/// usual cause is a slow-USB link that cannot carry usable RF). No receive chain
/// is running, so a stuck receive plane surfaces this state — carrying the
/// adapter's USB speed/degraded facts — instead of going silent while the run
/// loop retries. Distinct from `reg_blocked` (a regulatory-gate refusal) and from
/// `searching` (a chain is up, hunting for the channel).
pub const STATE_NO_INJECTION: &str = "no_injection";
/// No receive key is on disk, so there is nothing to receive and the run loop
/// blocks before it even looks for an adapter. Without a sidecar for this state a
/// blocked ground station published NOTHING AT ALL — no interface, no state, no
/// reason — and its half of an unlinked pair was invisible to every surface
/// except the journal. That is why a drone holding a key from a reflashed peer
/// and a ground station holding no key at all could sit facing each other for a
/// whole session with nothing on screen to say so. Distinct from `reg_blocked`
/// (the gate refused), `no_injection` (the adapter would not inject) and
/// `searching` (a chain is up, hunting).
pub const STATE_BLOCKED_UNPAIRED: &str = "blocked_unpaired";
/// The receive key is present but no receive adapter could be found: none
/// plugged in, or the one in use was removed. Written so the sidecar stops
/// carrying the previous generation's `active` body.
pub const STATE_NO_ADAPTER: &str = "no_adapter";

/// Data-plane RX `wfb_rx` args for the ground profile. `-l 1000` enables the
/// per-second stats lines on stdout (without it the monitor stays empty and the
/// link reports disabled).
///
/// `link_id` is the TRANSMITTING DRONE's (`link_id(fleet_id, slot)`): the ground
/// station runs one of these per registered slot, each with its own
/// `channel_id`, all bound to the same interfaces. That is legal because
/// `wfb_rx` opens a promiscuous, non-exclusive pcap handle and compiles a
/// per-instance BPF on `channel_id` (`vendor/wfb-ng/src/rx.cpp:70,84`), so N
/// receivers on one adapter each see only their own drone's frames.
///
/// `ifaces` are every receiving adapter, trailing in order: `wfb_rx` captures
/// on each and combines their frames before FEC decode, and numbers them in
/// this order in its per-antenna stats (`antenna_id = wlan_idx << 8 | chain`).
pub fn data_rx_args(
    ifaces: &[String],
    rx_key: &Path,
    channel_port: u16,
    link_id: u32,
) -> Vec<String> {
    rx_args("0", ifaces, rx_key, channel_port, link_id)
}

/// The shared shape of every ground receiver: radio port, link id, loopback
/// egress, key, stats interval, then every receiving interface.
fn rx_args(
    radio_port: &str,
    ifaces: &[String],
    rx_key: &Path,
    egress_port: u16,
    link_id: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        radio_port.into(),
        "-i".into(),
        link_id.to_string(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        egress_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-l".into(),
        "1000".into(),
    ];
    args.extend(ifaces.iter().cloned());
    args
}

/// GS aux RX `wfb_rx` args: radio_id 2 (the aux application stream the drone
/// radiates its status, identity, MAVLink and plugin frames on), decoded to
/// `aux_port`. Same shape as `data_rx_args` with the aux radio_id; the
/// asymmetric-by-direction aux pair means the GS receives on `-p 2` (the drone
/// egresses on p2), never p3.
///
/// `link_id` is the transmitting drone's — one instance per registered slot.
pub fn gs_aux_rx_args(
    ifaces: &[String],
    rx_key: &Path,
    aux_port: u16,
    link_id: u32,
) -> Vec<String> {
    rx_args("2", ifaces, rx_key, aux_port, link_id)
}

/// GS aux-uplink TX `wfb_tx` args: radio_id 3, loopback ingress `aux_tx_port`,
/// light FEC. The ground→drone half of the aux pair, and the mirror of
/// `gs_aux_rx_args`: the drone egresses on p2 and listens on p3, so the
/// ground listens on p2 and egresses on p3.
///
/// Without this the aux lane was downlink-only. The drone has always run
/// `wfb_rx -p 3` re-emitting to its own loopback, but nothing on the ground
/// ever transmitted on that radio_id, so a ground station could hear a drone
/// and never answer it — every byte a connected client sent toward the drone
/// was handed to the ground's own (absent) flight-controller writer and
/// silently dropped.
///
/// `link_id` is the ground station's own (`link_id(fleet_id, SLOT_GROUND)`).
/// There is exactly ONE of these regardless of fleet size: every drone's aux
/// receiver keys to slot 0, so a fleet-wide uplink is one transmission, not N.
///
/// `aux_tx_port` is the operator-configured ingress, NOT a constant. It used to
/// be the literal [`AUX_TX_PORT`] while the three processes that write into
/// this lane — the MAVLink router's uplink sender, the control surface's
/// relay-proxy egress, and the link-feedback emitter — all resolved it from
/// config. An operator who changed the setting moved all three writers and left
/// the transmitter on the old number, so every GCS command, every relay call
/// and every feedback sample went to a port nothing was reading, with no error
/// anywhere. Taking the resolved value means the transmitter and its writers
/// cannot disagree.
pub fn gs_aux_tx_args(
    iface: &str,
    rx_key: &Path,
    mcs_index: u8,
    link_id: u32,
    aux_tx_port: u16,
) -> Vec<String> {
    vec![
        "-p".into(),
        "3".into(),
        "-i".into(),
        link_id.to_string(),
        "-u".into(),
        aux_tx_port.to_string(),
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

/// GS rx-control `wfb_rx` args: radio_id 1, decode to `control_port`.
///
/// `link_id` is the transmitting drone's — one instance per registered slot,
/// each decoding to its own loopback port.
pub fn gs_rx_control_args(
    ifaces: &[String],
    rx_key: &Path,
    control_port: u16,
    link_id: u32,
) -> Vec<String> {
    rx_args("1", ifaces, rx_key, control_port, link_id)
}

/// GS tx-control `wfb_tx` args: radio_id 1, loopback ingress 5810, light FEC.
///
/// `link_id` is the ground station's own. Like the aux uplink there is exactly
/// ONE of these: a HopAck / PresenceBeacon reaches the whole fleet in one
/// transmission.
pub fn gs_tx_control_args(iface: &str, rx_key: &Path, mcs_index: u8, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-i".into(),
        link_id.to_string(),
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
mod tests;
