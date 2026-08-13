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
/// GS aux-uplink loopback ingress: whatever is written here is radiated on
/// radio_id 3, the ground→drone half of the aux pair. Mirrors the drone's
/// `aux_tx_port` default (the two rigs never share a host, so the same number
/// on both sides is the symmetric choice, not a collision). NOT per-slot, for
/// the same reason as [`TX_CONTROL_PORT`]: one uplink transmitter serves the
/// whole fleet. Deliberately outside the per-slot receive span so the uplink
/// ingress can never be fed by a downlink egress.
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

/// Data-plane RX `wfb_rx` args for the ground profile. `-l 1000` enables the
/// per-second stats lines on stdout (without it the monitor stays empty and the
/// link reports disabled).
///
/// `link_id` is the TRANSMITTING DRONE's (`link_id(fleet_id, slot)`): the ground
/// station runs one of these per registered slot, each with its own
/// `channel_id`, all bound to the same interface. That is legal because
/// `wfb_rx` opens a promiscuous, non-exclusive pcap handle and compiles a
/// per-instance BPF on `channel_id` (`vendor/wfb-ng/src/rx.cpp:70,84`), so N
/// receivers on one adapter each see only their own drone's frames.
pub fn data_rx_args(iface: &str, rx_key: &Path, channel_port: u16, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "0".into(),
        "-i".into(),
        link_id.to_string(),
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
///
/// `link_id` is the transmitting drone's — one instance per registered slot.
pub fn gs_atlas_rx_args(iface: &str, rx_key: &Path, atlas_port: u16, link_id: u32) -> Vec<String> {
    vec![
        "-p".into(),
        "2".into(),
        "-i".into(),
        link_id.to_string(),
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

/// GS aux-uplink TX `wfb_tx` args: radio_id 3, loopback ingress `aux_tx_port`,
/// light FEC. The ground→drone half of the aux pair, and the mirror of
/// `gs_atlas_rx_args`: the drone egresses on p2 and listens on p3, so the
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
    iface: &str,
    rx_key: &Path,
    control_port: u16,
    link_id: u32,
) -> Vec<String> {
    vec![
        "-p".into(),
        "1".into(),
        "-i".into(),
        link_id.to_string(),
        "-c".into(),
        "127.0.0.1".into(),
        "-u".into(),
        control_port.to_string(),
        "-K".into(),
        rx_key.to_string_lossy().into_owned(),
        "-l".into(),
        "1000".into(),
        iface.into(),
    ]
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
mod tests {
    use super::*;
    use ados_radio::config::{link_id, WfbConfig, FLEET_MAX_SLOTS, SLOT_GROUND};

    /// Read the value following `flag` in an arg vector.
    fn arg_after(args: &[String], flag: &str) -> String {
        args[args.iter().position(|x| x == flag).unwrap() + 1].clone()
    }

    #[test]
    fn data_rx_args_match_python() {
        // wfb_rx -p 0 -i <drone link> -c 127.0.0.1 -u <slot port> -K <rx.key> -l 1000 <iface>
        let a = data_rx_args(
            "wlan1",
            Path::new("/etc/ados/wfb/rx.key"),
            video_rx_port(1),
            link_id(1, 1),
        );
        assert_eq!(
            a,
            vec![
                "-p",
                "0",
                "-i",
                "257", // link_id(1, 1)
                "-c",
                "127.0.0.1",
                "-u",
                "5901",
                "-K",
                "/etc/ados/wfb/rx.key",
                "-l",
                "1000",
                "wlan1"
            ]
        );
    }

    #[test]
    fn gs_atlas_rx_uses_radio_id_2_and_the_slot_aux_port() {
        // The drone egresses Atlas events on the aux radio_id 2; the GS receives
        // on p2 (NOT p3), decoding to that slot's aux port.
        let a = gs_atlas_rx_args(
            "wlan1",
            Path::new("/etc/ados/wfb/rx.key"),
            aux_rx_port(3),
            link_id(1, 3),
        );
        assert_eq!(a[0], "-p");
        assert_eq!(a[1], "2");
        assert_eq!(arg_after(&a, "-u"), aux_rx_port(3).to_string());
        assert_eq!(a.last().unwrap(), "wlan1");
    }

    #[test]
    fn the_three_per_slot_egress_ranges_never_overlap_or_alias_a_fixed_port() {
        // The receive plane decodes into these ports and the consumers bind them.
        // An overlap silently crosses two lanes (video frames arriving at the aux
        // consumer); an alias onto a fixed transmit ingress feeds the radio its
        // own downlink. Both are silent — nothing errors, the lane just carries
        // the wrong bytes — so the disjointness is pinned here.
        let mut seen = std::collections::BTreeSet::new();
        for slot in 1..=FLEET_MAX_SLOTS {
            for port in [
                video_rx_port(slot),
                aux_rx_port(slot),
                control_rx_port(slot),
            ] {
                assert!(seen.insert(port), "duplicate egress port {port}");
                assert!(
                    ados_radio::config::is_fleet_rx_port(port),
                    "{port} must be inside the guarded fleet receive span"
                );
            }
        }
        assert_eq!(seen.len(), FLEET_MAX_SLOTS as usize * 3);
        // The two single-transmitter ingress ports sit outside the span, so an
        // operator aux-port guard rejecting the span cannot reject them.
        assert!(!seen.contains(&TX_CONTROL_PORT));
        assert!(!seen.contains(&AUX_TX_PORT));
        assert!(!ados_radio::config::is_fleet_rx_port(TX_CONTROL_PORT));
        assert!(!ados_radio::config::is_fleet_rx_port(AUX_TX_PORT));
    }

    #[test]
    fn the_uplink_transmitter_follows_the_configured_ingress_port() {
        // The regression this guards: the transmitter held a constant while the
        // three processes that write into this lane resolved the port from
        // config. An operator who changed the setting moved every writer and
        // left the transmitter behind, so GCS commands, relay calls and link
        // feedback all went to a port nothing was reading — with no error
        // anywhere, which reads as a dead radio.
        let a = gs_aux_tx_args("wlan1", Path::new("/k"), 1, link_id(1, SLOT_GROUND), 6002);
        assert_eq!(
            arg_after(&a, "-u"),
            "6002",
            "the transmitter must bind the operator's port, not a constant"
        );
    }

    #[test]
    fn the_transmitter_and_its_writers_resolve_the_same_port() {
        // The writers resolve through ados_protocol::aux_ports; the transmitter
        // takes the value passed to it. Same config text must yield the same
        // number on both sides, or the lane silently goes nowhere.
        let text = "video:\n  wfb:\n    aux_tx_port: 6002\n";
        let writers = ados_protocol::aux_ports::AuxPorts::from_yaml(text).tx;
        let a = gs_aux_tx_args(
            "wlan1",
            Path::new("/k"),
            1,
            link_id(1, SLOT_GROUND),
            writers,
        );
        assert_eq!(arg_after(&a, "-u"), writers.to_string());
        assert_eq!(writers, 6002);

        // And the defaults agree, so an unconfigured box is coherent too.
        let default_writers = ados_protocol::aux_ports::AuxPorts::default().tx;
        assert_eq!(
            default_writers, AUX_TX_PORT,
            "the transmitter's default and the writers' default must be one number"
        );
    }

    #[test]
    fn gs_aux_tx_uses_radio_id_3_and_the_uplink_ingress() {
        // The ground transmits the aux uplink on p3 (the radio_id the drone's
        // `wfb_rx` listens on), reading from its own loopback ingress, keyed to
        // the ground station's own slot so every drone hears it.
        let a = gs_aux_tx_args(
            "wlan1",
            Path::new("/k"),
            1,
            link_id(1, SLOT_GROUND),
            AUX_TX_PORT,
        );
        assert_eq!(a[0], "-p");
        assert_eq!(a[1], "3");
        assert_eq!(arg_after(&a, "-i"), link_id(1, SLOT_GROUND).to_string());
        assert_eq!(arg_after(&a, "-u"), AUX_TX_PORT.to_string());
        assert_eq!(arg_after(&a, "-k"), "1"); // light FEC, same ratio as the drone's aux
        assert_eq!(a.last().unwrap(), "wlan1");
    }

    #[test]
    fn the_aux_pair_is_opposite_on_the_ground_to_the_drone() {
        // The whole point of the pair: each rig receives on the radio_id its
        // peer transmits on. If these two ever end up equal the lane talks to
        // itself and the link goes silent in one direction with no error.
        let rx = gs_atlas_rx_args("wlan1", Path::new("/k"), aux_rx_port(1), link_id(1, 1));
        let tx = gs_aux_tx_args(
            "wlan1",
            Path::new("/k"),
            1,
            link_id(1, SLOT_GROUND),
            AUX_TX_PORT,
        );
        assert_eq!(rx[1], "2", "ground receives the downlink on p2");
        assert_eq!(tx[1], "3", "ground transmits the uplink on p3");
        assert_ne!(rx[1], tx[1]);
    }

    #[test]
    fn the_uplink_ingress_is_not_a_downlink_egress() {
        // Feeding the transmit ingress from a receive egress would loop every
        // decoded downlink frame straight back over the air.
        for slot in 1..=FLEET_MAX_SLOTS {
            assert_ne!(AUX_TX_PORT, aux_rx_port(slot));
            assert_ne!(TX_CONTROL_PORT, control_rx_port(slot));
        }
    }

    #[test]
    fn gs_rx_control_decodes_to_the_slot_control_port() {
        // The GS rx-control egress is per slot now; the drone side's mirror is
        // its own fixed 5810.
        let a = gs_rx_control_args("wlan1", Path::new("/k"), control_rx_port(2), link_id(1, 2));
        assert_eq!(arg_after(&a, "-u"), control_rx_port(2).to_string());
        assert_eq!(a[1], "1"); // radio_id 1
        assert_eq!(arg_after(&a, "-i"), link_id(1, 2).to_string());
    }

    #[test]
    fn gs_tx_control_uses_5810_and_light_fec() {
        let a = gs_tx_control_args("wlan1", Path::new("/k"), 3, link_id(1, SLOT_GROUND));
        assert_eq!(arg_after(&a, "-u"), "5810");
        assert_eq!(arg_after(&a, "-k"), "1"); // light FEC k=1
        assert_eq!(arg_after(&a, "-M"), "3"); // mcs passed through
        assert_eq!(arg_after(&a, "-i"), link_id(1, SLOT_GROUND).to_string());
    }

    #[test]
    fn the_ground_uplink_and_a_drone_downlink_never_share_a_channel_id() {
        // The ground's two transmitters key to slot 0; every drone receiver keys
        // to that drone's slot. If a ground transmitter ever picked up a drone's
        // link_id the two would share a channel_id and thrash each other's FEC
        // session — the failure that presents as unexplained link loss.
        let ground = link_id(1, SLOT_GROUND);
        let ctrl = arg_after(
            &gs_tx_control_args("wlan1", Path::new("/k"), 1, ground),
            "-i",
        );
        let aux = arg_after(
            &gs_aux_tx_args("wlan1", Path::new("/k"), 1, ground, AUX_TX_PORT),
            "-i",
        );
        assert_eq!(ctrl, ground.to_string());
        assert_eq!(aux, ground.to_string());
        for slot in 1..=FLEET_MAX_SLOTS {
            let drone = link_id(1, slot);
            assert_ne!(ctrl, drone.to_string());
            let video = arg_after(
                &data_rx_args("wlan1", Path::new("/k"), video_rx_port(slot), drone),
                "-i",
            );
            assert_eq!(video, drone.to_string());
        }
    }

    #[test]
    fn every_receive_builder_emits_the_link_id_after_the_radio_port() {
        // Same invariant as the drone side: a builder extended without threading
        // the id silently falls back to wfb-ng's default link_id 0.
        let drone = link_id(4, 6);
        let ground = link_id(4, SLOT_GROUND);
        let key = Path::new("/k");
        let cases: [(&str, Vec<String>, u32); 5] = [
            (
                "data_rx",
                data_rx_args("wlan1", key, video_rx_port(6), drone),
                drone,
            ),
            (
                "atlas_rx",
                gs_atlas_rx_args("wlan1", key, aux_rx_port(6), drone),
                drone,
            ),
            (
                "rx_control",
                gs_rx_control_args("wlan1", key, control_rx_port(6), drone),
                drone,
            ),
            (
                "aux_tx",
                gs_aux_tx_args("wlan1", key, 1, ground, AUX_TX_PORT),
                ground,
            ),
            (
                "tx_control",
                gs_tx_control_args("wlan1", key, 1, ground),
                ground,
            ),
        ];
        for (name, args, want) in cases {
            let p = args
                .iter()
                .position(|x| x == "-p")
                .unwrap_or_else(|| panic!("{name}: no -p"));
            assert_eq!(args[p + 2], "-i", "{name}: -i must follow the -p pair");
            assert_eq!(args[p + 3], want.to_string(), "{name}: wrong link_id");
        }
    }

    #[test]
    fn default_reg_domain_matches_air_side() {
        // The GS default regulatory domain must equal the air side's so both
        // rigs enable the same channel set (the home channel 149 is permitted).
        assert_eq!(DEFAULT_REG_DOMAIN, "US");
        assert_eq!(DEFAULT_REG_DOMAIN, WfbConfig::default().reg_domain.unwrap());
    }
}
