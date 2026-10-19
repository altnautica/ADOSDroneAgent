//! Receive-chain arg builder tests.

use super::*;
use ados_radio::config::{link_id, WfbConfig, FLEET_MAX_SLOTS, SLOT_GROUND};

/// Read the value following `flag` in an arg vector.
fn arg_after(args: &[String], flag: &str) -> String {
    args[args.iter().position(|x| x == flag).unwrap() + 1].clone()
}

/// A single-adapter interface list.
fn one(iface: &str) -> Vec<String> {
    vec![iface.to_string()]
}

#[test]
fn data_rx_args_match_python() {
    // wfb_rx -p 0 -i <drone link> -c 127.0.0.1 -u <slot port> -K <rx.key> -l 1000 <iface>
    let a = data_rx_args(
        &one("wlan1"),
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

fn detected(i: u8) -> ados_radio::adapter::DetectedAdapter {
    ados_radio::adapter::DetectedAdapter {
        ifname: format!("wlan{i}"),
        mac: Some(format!("aa:bb:cc:dd:ee:0{i}")),
        usb_path: Some(format!("1-1.{i}")),
        driver: "rtl88x2eu".into(),
        chipset: "RTL8812EU".into(),
        verified: true,
        usb_speed_mbps: Some(480),
        usb_degraded: false,
    }
}

#[test]
fn every_receiver_listens_on_each_rx_adapter_in_order_and_skips_mesh_and_tx_only() {
    use ados_radio::adapter::{resolve_roles, AdapterPlan};
    use ados_radio::config::{AdapterEntry, AdapterMatch, AdapterRole, RadioAdaptersConfig};
    // Five adapters: wlan2 transmits only, wlan5 is the mesh carrier, the
    // rest receive. wfb_rx gets the receivers in detection order.
    let entry = |usb: &str, role| AdapterEntry {
        matcher: AdapterMatch {
            mac: None,
            usb_path: Some(usb.into()),
        },
        role,
        antenna: None,
    };
    let cfg = RadioAdaptersConfig {
        adapters: vec![
            entry("1-1.1", AdapterRole::Rxtx),
            entry("1-1.2", AdapterRole::Tx),
            entry("1-1.5", AdapterRole::Mesh),
        ],
    };
    let adapters: Vec<_> = (1..=5).map(detected).collect();
    let plan = AdapterPlan::new(resolve_roles(&adapters, &cfg, false)).unwrap();
    let rx = plan.rx_ifaces();
    let key = Path::new("/k");
    for args in [
        data_rx_args(&rx, key, video_rx_port(1), link_id(1, 1)),
        gs_aux_rx_args(&rx, key, aux_rx_port(1), link_id(1, 1)),
        gs_rx_control_args(&rx, key, control_rx_port(1), link_id(1, 1)),
    ] {
        let after_stats = args.iter().position(|a| a == "1000").unwrap() + 1;
        assert_eq!(&args[after_stats..], ["wlan1", "wlan3", "wlan4"]);
    }
    // The transmitters run on the tx adapter alone.
    let tx = gs_tx_control_args(plan.tx().ifname(), key, 1, link_id(1, SLOT_GROUND));
    assert_eq!(tx.last().unwrap(), "wlan2");

    // Automatic roles with the mesh on: the last adapter is held back.
    let auto = AdapterPlan::new(resolve_roles(
        &adapters,
        &RadioAdaptersConfig::default(),
        true,
    ))
    .unwrap();
    let args = data_rx_args(&auto.rx_ifaces(), key, video_rx_port(1), link_id(1, 1));
    assert_eq!(
        &args[args.len() - 4..],
        ["wlan1", "wlan2", "wlan3", "wlan4"]
    );
    assert_eq!(auto.tx().ifname(), "wlan1");
}

#[test]
fn gs_aux_rx_uses_radio_id_2_and_the_slot_aux_port() {
    // The drone egresses its aux frames on radio_id 2; the GS receives
    // on p2 (NOT p3), decoding to that slot's aux port.
    let a = gs_aux_rx_args(
        &one("wlan1"),
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
    let rx = gs_aux_rx_args(
        &one("wlan1"),
        Path::new("/k"),
        aux_rx_port(1),
        link_id(1, 1),
    );
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
    let a = gs_rx_control_args(
        &one("wlan1"),
        Path::new("/k"),
        control_rx_port(2),
        link_id(1, 2),
    );
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
            &data_rx_args(&one("wlan1"), Path::new("/k"), video_rx_port(slot), drone),
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
            data_rx_args(&one("wlan1"), key, video_rx_port(6), drone),
            drone,
        ),
        (
            "aux_rx",
            gs_aux_rx_args(&one("wlan1"), key, aux_rx_port(6), drone),
            drone,
        ),
        (
            "rx_control",
            gs_rx_control_args(&one("wlan1"), key, control_rx_port(6), drone),
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
