use super::lane::{AnyTransport, Lane, TxOutcome};
use super::*;
use crate::config::LoraConfig;
use crate::frame::Beacon;
use crate::governor::power::Antenna;
use crate::governor::{Governor, Refusal, Setup};
use crate::ledger::Ledger;
use crate::regions::{by_code, RegionSource, Resolved};
use crate::transport::loopback::Loopback;
use crate::transport::Transport;
use tokio::net::UdpSocket;

fn fixed_now() -> Now {
    Now {
        mono_ns: 1_000_000_000,
        utc_ms: 1_760_000_000_000,
        utc_trusted: false,
    }
}

/// A loopback lane talking to a test peer socket.
async fn loopback_lane(dir: &std::path::Path, code: Option<&str>) -> (Lane, UdpSocket) {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = LoraConfig::default();
    cfg.loopback.bind = "127.0.0.1:0".into();
    cfg.loopback.peer = peer.local_addr().unwrap().to_string();
    let t = Loopback::open(&cfg).await.unwrap();
    let resolved = code.map(|c| Resolved {
        plan: by_code(c).unwrap(),
        source: RegionSource::LoraRegion,
    });
    let (ledger, _) = Ledger::open(&dir.join("airtime.jsonl"), "b", fixed_now(), 3600).unwrap();
    let governor = Governor::new(
        Setup {
            resolved,
            caps: t.caps().clone(),
            modem: t.modem(),
            airtime_factor: 1,
            freq_mhz: None,
            erp_mw: None,
            antenna: Antenna {
                gain_dbi: 2.15,
                cable_loss_db: 0.0,
            },
            node_id: 9,
        },
        ledger,
    );
    let lane = Lane {
        transport: AnyTransport::Loopback(t),
        governor,
        node_id: 9,
        rx_frames: 0,
        rx_crc_errors: 0,
        last_heard: None,
    };
    (lane, peer)
}

fn beacon_frame(node: u16) -> Frame {
    Frame {
        node_id: node,
        kind: Kind::Beacon,
        payload: Beacon {
            position: Some((12.5, 77.5)),
            alt_m: Some(900.0),
            battery_pct: Some(80),
            mode: "AUTO".into(),
        }
        .to_payload(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn lane_addr(lane: &Lane) -> std::net::SocketAddr {
    match &lane.transport {
        AnyTransport::Loopback(l) => l.local_addr().unwrap(),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn japan_refuses_lbt_busy_after_three_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let (mut lane, peer) = loopback_lane(dir.path(), Some("JP")).await;
    let addr = lane_addr(&lane).await;
    // The peer raises the ambient level above -80 dBm, then sends a frame
    // so the lane's receive side has applied it.
    peer.send_to(br#"{"ambient_rssi_dbm": -70}"#, addr)
        .await
        .unwrap();
    let f = beacon_frame(42).encode().unwrap();
    let msg = format!(
        r#"{{"frame_hex": "{}", "rssi_dbm": -97, "snr_db": 6.5}}"#,
        hex(&f)
    );
    peer.send_to(msg.as_bytes(), addr).await.unwrap();
    let ev = lane.transport_recv().await.unwrap();
    assert!(lane.on_rx(ev, fixed_now()).is_some());

    let out = lane.transmit(&beacon_frame(9), fixed_now).await.unwrap();
    assert_eq!(out, TxOutcome::Refused(Refusal::LbtBusy));
    let st = lane.governor.status(23, fixed_now());
    assert_eq!(st.blocked_reason, Some("lbt_busy"));
    let lbt = st.governor["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "lbt")
        .unwrap()
        .clone();
    assert_eq!(lbt["busy_count"], 3);
    assert_eq!(lbt["last_rssi_dbm"], -70);
    assert_eq!(st.tx["refused_by_rule"]["lbt"], 1);

    // Quiet again: the next frame goes out.
    peer.send_to(br#"{"ambient_rssi_dbm": null}"#, addr)
        .await
        .unwrap();
    peer.send_to(msg.as_bytes(), addr).await.unwrap();
    let ev = lane.transport_recv().await.unwrap();
    lane.on_rx(ev, fixed_now());
    let out = lane.transmit(&beacon_frame(9), fixed_now).await.unwrap();
    assert_eq!(out, TxOutcome::Sent);
    let mut buf = [0u8; 128];
    let (n, _) = peer.recv_from(&mut buf).await.unwrap();
    assert_eq!(Frame::decode(&buf[..n]).unwrap().0.node_id, 9);
}

#[tokio::test]
async fn region_unset_blocks_transmit_but_still_lists_received_beacons() {
    let dir = tempfile::tempdir().unwrap();
    let (mut lane, peer) = loopback_lane(dir.path(), None).await;
    let addr = lane_addr(&lane).await;
    let out = lane.transmit(&beacon_frame(9), fixed_now).await.unwrap();
    assert_eq!(out, TxOutcome::Refused(Refusal::RegionUnset));

    peer.send_to(&beacon_frame(42).encode().unwrap(), addr)
        .await
        .unwrap();
    let ev = lane.transport_recv().await.unwrap();
    let entry = lane.on_rx(ev, fixed_now()).unwrap();
    assert_eq!(entry["node_id"], 42);
    assert_eq!(entry["kind"], "beacon");
    assert_eq!(entry["mode"], "AUTO");
    assert_eq!(entry["battery_pct"], 80);
    assert_eq!(lane.rx_frames, 1);
}

#[test]
fn the_india_beacon_interval_stretches_for_the_e22_charge() {
    // 411.648 ms charged per beacon against a 36 s budget at 80 %.
    assert_eq!(beacon_interval_s(30, 411_648, Some(36.0)), 52);
    assert_eq!(beacon_interval_s(60, 411_648, Some(36.0)), 60);
    assert_eq!(beacon_interval_s(30, 411_648, None), 30);
}
