use super::*;
use crate::config::{link_id, FLEET_MAX_SLOTS, SLOT_GROUND};

/// A plane that writes more than a pipe's worth of stderr must keep running
/// to completion. `wfb_tx` prints a dropped-packets line every stats interval
/// while congested; with stderr on an undrained pipe it blocks in
/// `fprintf(stderr)` once 64 KiB accumulate and stops transmitting.
#[tokio::test]
async fn a_plane_never_blocks_on_its_own_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("plane.log");
    let args = vec![
        "-c".to_string(),
        "head -c 262144 /dev/zero >&2; exit 0".to_string(),
    ];
    let mut p = WfbProcess::spawn_in_group("sh", &args, log.to_str().unwrap())
        .await
        .unwrap();
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), p.child.wait())
        .await
        .expect("a plane writing 256 KiB of stderr must not block");
    assert!(exited.unwrap().success());
    assert_eq!(std::fs::metadata(&log).unwrap().len(), 262_144);
}

/// Capping a running plane's log empties it for good: the plane keeps
/// writing at the new end of the file rather than at its old offset, which
/// would leave the file as large as before with a hole at the front.
#[tokio::test]
async fn a_capped_log_restarts_small_under_a_running_plane() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("plane.log");
    let go = dir.path().join("go");
    let script = format!(
        "head -c 4096 /dev/zero >&2; while [ ! -e '{}' ]; do sleep 0.02; done; \
         head -c 10 /dev/zero >&2",
        go.display()
    );
    let args = vec!["-c".to_string(), script];
    let mut p = WfbProcess::spawn_in_group("sh", &args, log.to_str().unwrap())
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::metadata(&log).unwrap().len() < 4096 {
        assert!(
            std::time::Instant::now() < deadline,
            "the plane never wrote"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !cap_log(&log, 4096).await.unwrap(),
        "at the cap is not over it"
    );
    assert!(cap_log(&log, 1024).await.unwrap());
    std::fs::write(&go, b"").unwrap();
    let exited = tokio::time::timeout(std::time::Duration::from_secs(10), p.child.wait())
        .await
        .expect("the plane finishes");
    assert!(exited.unwrap().success());
    assert_eq!(std::fs::metadata(&log).unwrap().len(), 10);
}

/// A transmitter's own per-second stats lines accumulate into its plane's
/// totals: the lines themselves (the loop is turning) and the per-interval
/// ingress and injection bytes, summed.
#[tokio::test]
async fn tx_plane_stats_lines_accumulate_into_the_plane_totals() {
    let mut child = tokio::process::Command::new("sh")
        .args([
            "-c",
            "printf '1\\tPKT\\t0:2:200:2:230:0:0\\nSession restarted\\n2\\tPKT\\t0:1:100:1:115:0:0\\n'",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stats = Arc::new(TxPlaneCounters::default());
    tx_stats_reader_loop(child.stdout.take().unwrap(), stats.clone()).await;
    let _ = child.wait().await;
    assert_eq!(
        stats.totals(),
        TxPlaneTotals {
            lines: 2,
            bytes_in: 300,
            bytes_injected: 345,
        }
    );
}

/// Read the value following `flag` in an arg vector.
fn arg_after(args: &[String], flag: &str) -> String {
    args[args.iter().position(|x| x == flag).unwrap() + 1].clone()
}

/// The wfb-ng `channel_id` an arg vector resolves to:
/// `(link_id << 8) + radio_port`. This is the value `rx.cpp` compiles into
/// its per-instance BPF, so two arg vectors sharing one is two transmitters
/// thrashing each other's FEC decoder.
fn channel_id_of(args: &[String]) -> u64 {
    let link: u64 = arg_after(args, "-i").parse().unwrap();
    let port: u64 = arg_after(args, "-p").parse().unwrap();
    (link << 8) + port
}

/// Every `wfb_tx` plane binds a management socket, and each one's `-C` port
/// MUST be `control_port` of that plane's own `-p` radio port. A drift here
/// would send a data-plane retune to the control plane's transmitter — the
/// video would keep its old rate while HopAck traffic silently changed rung.
/// The three `wfb_rx` builders are deliberately excluded: `-C` is a `wfb_tx`
/// flag and `wfb_rx` would reject it.
#[test]
fn every_tx_plane_binds_its_own_management_port() {
    let cfg = WfbConfig::default();
    let key = Path::new("/etc/ados/wfb/tx.key");
    let planes = [
        data_tx_args("wlan1", &cfg, key, link_id(1, 3)),
        tx_control_args("wlan1", &cfg, key, link_id(1, 3)),
        aux_tx_args("wlan1", &cfg, key, link_id(1, 3)),
    ];
    let mut seen = Vec::new();
    for args in &planes {
        let radio_port: u8 = arg_after(args, "-p").parse().unwrap();
        let ctl: u16 = arg_after(args, "-C").parse().unwrap();
        assert_eq!(
            ctl,
            crate::tx_cmd::control_port(radio_port),
            "radio port {radio_port} bound the wrong management port"
        );
        seen.push(ctl);
    }
    // And no two planes share one, or a retune would hit both.
    seen.sort_unstable();
    let uniq = seen.len();
    seen.dedup();
    assert_eq!(seen.len(), uniq, "two tx planes share a management port");
    // The data plane is the one the adaptive ladder drives.
    assert_eq!(
        crate::tx_cmd::control_port(DATA_RADIO_PORT),
        arg_after(&planes[0], "-C").parse::<u16>().unwrap()
    );
}

/// The two `wfb_rx` builders must NOT carry `-C`.
#[test]
fn receive_planes_carry_no_management_port() {
    let cfg = WfbConfig::default();
    let key = Path::new("/etc/ados/wfb/rx.key");
    for args in [
        rx_control_args("wlan1", key, link_id(1, 0)),
        aux_rx_args("wlan1", &cfg, key, link_id(1, 0)),
    ] {
        assert!(
            !args.iter().any(|a| a == "-C"),
            "wfb_rx has no -C option: {args:?}"
        );
    }
}

#[test]
fn respawn_data_cfg_reuses_retained_tunables_not_boot_config() {
    // Boot config carries the default tier (k=8, n=12, mcs=1). The live data
    // plane has since moved to a manual / adaptive tier (k=4, n=8, mcs=5). A
    // whole-group respawn (a channel hop) must spawn the data plane from the
    // RETAINED tier, not silently revert it to the boot config.
    let boot = WfbConfig {
        fec_k: 8,
        fec_n: 12,
        mcs_index: 1,
        channel: 149,
        ..WfbConfig::default()
    };
    let data_cfg = data_cfg_from_retained(&boot, 4, 8, 5);
    // The data tier is the retained trio.
    assert_eq!(data_cfg.fec_k, 4);
    assert_eq!(data_cfg.fec_n, 8);
    assert_eq!(data_cfg.mcs_index, 5);
    // Everything else still comes from the boot config (channel preserved).
    assert_eq!(data_cfg.channel, 149);
    // The data-plane args carry the retained trio, not the boot defaults.
    let args = data_tx_args(
        "wlan1",
        &data_cfg,
        Path::new("/etc/ados/wfb/tx.key"),
        link_id(1, 3),
    );
    assert_eq!(arg_after(&args, "-k"), "4");
    assert_eq!(arg_after(&args, "-n"), "8");
    assert_eq!(arg_after(&args, "-M"), "5");
}

#[test]
fn data_tx_args_layout() {
    let cfg = WfbConfig::default();
    let a = data_tx_args(
        "wlan1",
        &cfg,
        Path::new("/etc/ados/wfb/tx.key"),
        link_id(1, 3),
    );
    assert_eq!(
        a,
        vec![
            "-p",
            "0",
            "-i",
            "259", // link_id(1, 3) = (1 << 8) | 3
            "-C",
            "8000", // TX_CMD_PORT_BASE + radio_port 0
            "-u",
            "5600",
            "-K",
            "/etc/ados/wfb/tx.key",
            "-k",
            "8",
            "-n",
            "12",
            "-B",
            "20",
            "-M",
            "1",
            "wlan1"
        ]
    );
}

#[test]
fn tx_control_args_layout() {
    // wfb_tx -p 1 -i <link> -C 8001 -u 5803 -K <key> -k 1 -n 2 -B 20 -M <mcs> <iface>
    let cfg = WfbConfig::default();
    let a = tx_control_args(
        "wlan1",
        &cfg,
        Path::new("/etc/ados/wfb/tx.key"),
        link_id(1, 3),
    );
    assert_eq!(
        a,
        vec![
            "-p",
            "1",
            "-i",
            "259",
            "-C",
            "8001", // TX_CMD_PORT_BASE + radio_port 1
            "-u",
            "5803",
            "-K",
            "/etc/ados/wfb/tx.key",
            "-k",
            "1",
            "-n",
            "2",
            "-B",
            "20",
            "-M",
            "1",
            "wlan1"
        ]
    );
}

#[test]
fn rx_control_args_layout() {
    // wfb_rx -p 1 -i <ground link> -c 127.0.0.1 -u 5810 -K <key> -l 1000 <iface>
    let a = rx_control_args(
        "wlan1",
        Path::new("/etc/ados/wfb/tx.key"),
        link_id(1, SLOT_GROUND),
    );
    assert_eq!(
        a,
        vec![
            "-p",
            "1",
            "-i",
            "256", // link_id(1, 0) — the shared uplink, not this drone's slot
            "-c",
            "127.0.0.1",
            "-u",
            "5810",
            "-K",
            "/etc/ados/wfb/tx.key",
            "-l",
            "1000",
            "wlan1"
        ]
    );
}

#[test]
fn fec_ratio_guard_rejects_degenerate_ratios() {
    // Valid: positive k with at least one parity shard.
    assert!(fec_ratio_valid(8, 12));
    assert!(fec_ratio_valid(8, 10));
    assert!(fec_ratio_valid(1, 2));
    // k = 0 is rejected.
    assert!(!fec_ratio_valid(0, 4));
    // n <= k is rejected (no parity shards).
    assert!(!fec_ratio_valid(8, 8));
    assert!(!fec_ratio_valid(8, 4));
}

#[test]
fn mcs_guard_accepts_0_through_7() {
    for mcs in 0..=7u8 {
        assert!(mcs_index_valid(mcs), "mcs {mcs} should be valid");
    }
    assert!(!mcs_index_valid(8));
    assert!(!mcs_index_valid(255));
}

#[test]
fn data_tx_args_track_set_fec_inputs() {
    // The data-tx args must carry whatever (k, n, mcs) trio is asked for, so
    // a respawn after set_fec/set_mcs injects the new tunables. This proves
    // the arg wiring the respawn relies on, without spawning a process.
    let cfg = WfbConfig {
        fec_k: 8,
        fec_n: 10,
        mcs_index: 5,
        ..WfbConfig::default()
    };
    let a = data_tx_args(
        "wlan1",
        &cfg,
        Path::new("/etc/ados/wfb/tx.key"),
        link_id(1, 1),
    );
    assert_eq!(arg_after(&a, "-k"), "8");
    assert_eq!(arg_after(&a, "-n"), "10");
    assert_eq!(arg_after(&a, "-M"), "5");
}

#[test]
fn manual_tier_validation_rejects_bad_inputs() {
    // The manual-tier setter gates on the SAME guards as set_mcs + set_fec
    // before it touches the data plane: an out-of-range MCS or an invalid FEC
    // ratio must be rejected up front so a respawn never carries bad args.
    // Exercise the two guard predicates the setter calls (no process spawn).
    assert!(mcs_index_valid(5) && fec_ratio_valid(8, 10)); // a valid trio
    assert!(!mcs_index_valid(9)); // MCS out of the 0..=7 range
    assert!(!fec_ratio_valid(8, 8)); // no parity shard
    assert!(!fec_ratio_valid(0, 4)); // zero data shards
}

#[test]
fn control_planes_use_lighter_fec_than_data() {
    let cfg = WfbConfig::default();
    let link = link_id(1, 1);
    let data = data_tx_args("wlan1", &cfg, Path::new("/k"), link);
    let ctrl = tx_control_args("wlan1", &cfg, Path::new("/k"), link);
    // data plane: k=8 n=12; control plane: k=1 n=2.
    assert_eq!(arg_after(&data, "-k"), "8");
    assert_eq!(arg_after(&ctrl, "-k"), "1");
}

#[test]
fn aux_tx_args_use_radio_id_2_and_aux_port_and_fec() {
    // The aux tx must sit on radio_id 2 (separate from data 0 / control 1)
    // with the aux ingress port and the light aux FEC, riding the data-plane
    // MCS by default.
    let cfg = WfbConfig::default();
    let a = aux_tx_args(
        "wlan1",
        &cfg,
        Path::new("/etc/ados/wfb/tx.key"),
        link_id(1, 1),
    );
    assert_eq!(arg_after(&a, "-p"), "2");
    assert_eq!(arg_after(&a, "-u"), cfg.aux_tx_port.to_string());
    assert_eq!(arg_after(&a, "-k"), cfg.aux_fec_k.to_string());
    assert_eq!(arg_after(&a, "-n"), cfg.aux_fec_n.to_string());
    // No aux MCS override → the aux pair rides the data-plane rate.
    assert_eq!(arg_after(&a, "-M"), cfg.mcs_index.to_string());
    assert_eq!(a.last().unwrap(), "wlan1");
}

#[test]
fn aux_rx_args_use_radio_id_3_and_loopback_reemit() {
    // The aux rx must sit on radio_id 3 and re-emit on loopback at the aux rx
    // port — distinct from the aux tx ingress so receive can't feed transmit.
    let cfg = WfbConfig::default();
    let a = aux_rx_args(
        "wlan1",
        &cfg,
        Path::new("/etc/ados/wfb/tx.key"),
        link_id(1, SLOT_GROUND),
    );
    assert_eq!(arg_after(&a, "-p"), "3");
    assert_eq!(arg_after(&a, "-c"), "127.0.0.1");
    assert_eq!(arg_after(&a, "-u"), cfg.aux_rx_port.to_string());
    // The re-emit port differs from the tx ingress.
    assert_ne!(cfg.aux_rx_port, cfg.aux_tx_port);
}

#[test]
fn aux_tx_args_honour_an_explicit_aux_mcs_override() {
    let cfg = WfbConfig {
        mcs_index: 1,
        aux_mcs_index: Some(5),
        ..WfbConfig::default()
    };
    let a = aux_tx_args("wlan1", &cfg, Path::new("/k"), link_id(1, 1));
    // The aux pair uses the override, not the data-plane MCS.
    assert_eq!(arg_after(&a, "-M"), "5");
}

#[test]
fn aux_radio_ids_never_collide_with_data_or_control_planes() {
    // The four wfb planes claim four distinct radio_ids on the shared adapter:
    // data 0, control 1, aux tx 2, aux rx 3. A collision would corrupt one
    // plane's frames, so this is a hard invariant.
    let cfg = WfbConfig::default();
    let key = Path::new("/k");
    let own = link_id(1, 1);
    let up = link_id(1, SLOT_GROUND);
    let ids = [
        arg_after(&data_tx_args("wlan1", &cfg, key, own), "-p"),
        arg_after(&tx_control_args("wlan1", &cfg, key, own), "-p"),
        arg_after(&aux_tx_args("wlan1", &cfg, key, own), "-p"),
        arg_after(&aux_rx_args("wlan1", &cfg, key, up), "-p"),
    ];
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            assert_ne!(ids[i], ids[j], "radio_id collision between planes");
        }
    }
}

#[test]
fn aux_settings_round_trip_through_a_cfg_view() {
    // The retained aux settings rebuild a cfg view that reproduces the same
    // aux args on a respawn — the channel-hop preservation path.
    let s = AuxSettings {
        tx_port: 5620,
        rx_port: 5621,
        fec_k: 2,
        fec_n: 4,
        mcs_index: 3,
    };
    let cfg = s.to_cfg();
    let a = aux_tx_args("wlan1", &cfg, Path::new("/k"), link_id(1, 1));
    assert_eq!(arg_after(&a, "-u"), "5620");
    assert_eq!(arg_after(&a, "-k"), "2");
    assert_eq!(arg_after(&a, "-n"), "4");
    assert_eq!(arg_after(&a, "-M"), "3");
    let r = aux_rx_args("wlan1", &cfg, Path::new("/k"), link_id(1, SLOT_GROUND));
    assert_eq!(arg_after(&r, "-u"), "5621");
}

#[test]
fn every_builder_emits_the_link_id_immediately_after_the_radio_port() {
    // `-i` must follow the `-p` pair in every builder. Position is not
    // cosmetic here: an arg vector that carries `-p` but drops `-i` silently
    // falls back to wfb-ng's default link_id 0, which every un-keyed process
    // in radio range also uses — the exact FEC-thrash collision the fleet
    // addressing exists to prevent. A positional assertion catches a builder
    // that was extended without threading the id.
    let cfg = WfbConfig::default();
    let key = Path::new("/k");
    let own = link_id(7, 9);
    let up = link_id(7, SLOT_GROUND);
    let cases: [(&str, Vec<String>, u32); 5] = [
        ("data_tx", data_tx_args("wlan1", &cfg, key, own), own),
        ("tx_control", tx_control_args("wlan1", &cfg, key, own), own),
        ("rx_control", rx_control_args("wlan1", key, up), up),
        ("aux_tx", aux_tx_args("wlan1", &cfg, key, own), own),
        ("aux_rx", aux_rx_args("wlan1", &cfg, key, up), up),
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
fn drone_transmit_and_uplink_receive_carry_different_link_ids() {
    // The asymmetry the whole fleet design rests on: a drone TRANSMITS under
    // its own slot but RECEIVES the ground station's single shared uplink.
    // If the receive side were keyed to the drone's own slot it would listen
    // to a channel_id nobody transmits on and the uplink would go silent;
    // if the transmit side were keyed to slot 0 every drone in the fleet
    // would collide on one channel_id.
    let cfg = WfbConfig::default();
    let key = Path::new("/k");
    let own = link_id(1, 5);
    let up = link_id(1, SLOT_GROUND);
    assert_ne!(own, up);

    let tx = arg_after(&tx_control_args("wlan1", &cfg, key, own), "-i");
    let rx = arg_after(&rx_control_args("wlan1", key, up), "-i");
    assert_eq!(tx, own.to_string());
    assert_eq!(rx, up.to_string());
    assert_ne!(tx, rx, "control TX and control RX must not share a link_id");

    let aux_tx = arg_after(&aux_tx_args("wlan1", &cfg, key, own), "-i");
    let aux_rx = arg_after(&aux_rx_args("wlan1", &cfg, key, up), "-i");
    assert_ne!(aux_tx, aux_rx, "aux TX and aux RX must not share a link_id");

    // Every receiver — including the one the link block is measured from —
    // is keyed to the uplink. A drone cannot measure its own downlink: one
    // radio in monitor mode does not capture its own injected frames, so a
    // receiver keyed to this drone's own slot would decode nothing at all.
    let stats = arg_after(&rx_control_args("wlan1", key, up), "-i");
    assert_eq!(stats, up.to_string());
    assert_ne!(
        stats,
        own.to_string(),
        "the link-stats receiver must never be keyed to this drone's own slot"
    );
}

#[test]
fn no_two_slots_in_a_fleet_share_a_transmit_channel_id() {
    // Every drone's four planes must land on their own channel_id across the
    // whole 24-slot fleet plus the ground station's uplink pair. Asserted at
    // the argv level (what the process actually receives), not just on the
    // `link_id` helper.
    let cfg = WfbConfig::default();
    let key = Path::new("/k");
    let fleet = 1u16;
    let up = link_id(fleet, SLOT_GROUND);
    let mut seen = std::collections::BTreeSet::new();

    // The ground station's two uplink transmitters (control p1, aux p3).
    assert!(seen.insert(channel_id_of(&tx_control_args("wlan1", &cfg, key, up))));
    assert!(seen.insert(channel_id_of(&aux_tx_args("wlan1", &cfg, key, up))));

    for slot in 1..=FLEET_MAX_SLOTS {
        let own = link_id(fleet, slot);
        for args in [
            data_tx_args("wlan1", &cfg, key, own),
            tx_control_args("wlan1", &cfg, key, own),
            aux_tx_args("wlan1", &cfg, key, own),
        ] {
            let ch = channel_id_of(&args);
            assert!(seen.insert(ch), "duplicate channel_id {ch} at slot {slot}");
        }
        // Every receiver is keyed to the uplink, so no receiver contributes a
        // transmit channel_id here. A drone's own transmit channel_ids must
        // stay disjoint from the ground station's uplink pair asserted above.
    }
    // 2 ground uplink transmitters + 24 slots x 3 distinct transmit planes.
    assert_eq!(seen.len(), 2 + FLEET_MAX_SLOTS as usize * 3);
}
