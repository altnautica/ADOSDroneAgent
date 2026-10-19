use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

use super::pb::{fields, put_bytes, put_fixed32, put_float, put_int32, put_uint};
use super::*;
use crate::frame::Status;

const ME: u32 = 0x1234_5678;
const OTHER: u32 = 0x0BAD_CAFE;

fn from_radio(field: u32, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::new();
    put_uint(&mut m, 1, 99);
    put_bytes(&mut m, field, body);
    msg::frame(&m)
}

fn lora_config() -> Vec<u8> {
    let mut l = Vec::new();
    put_uint(&mut l, msg::lora::USE_PRESET, 1);
    put_uint(&mut l, msg::lora::MODEM_PRESET, 4); // MEDIUM_FAST
    put_uint(&mut l, msg::lora::REGION, 3);
    put_uint(&mut l, msg::lora::HOP_LIMIT, 5);
    put_uint(&mut l, msg::lora::TX_ENABLED, 1);
    put_int32(&mut l, msg::lora::TX_POWER, 27);
    put_uint(&mut l, 11, 20); // channel_num, kept as is
    l
}

/// The node's side of a config download for a Heltec V3.
fn handshake_stream(config_id: u32) -> Vec<u8> {
    let mut out = b"boot console text\r\n".to_vec();
    let mut my = Vec::new();
    put_uint(&mut my, 1, u64::from(ME));
    out.extend(from_radio(3, &my));
    let mut meta = Vec::new();
    put_bytes(&mut meta, 1, b"2.6.0");
    put_uint(&mut meta, 9, 43); // HELTEC_V3
    out.extend(from_radio(13, &meta));
    let mut config = Vec::new();
    put_bytes(&mut config, 6, &lora_config());
    out.extend(from_radio(5, &config));
    let mut done = Vec::new();
    put_uint(&mut done, 7, u64::from(config_id));
    out.extend(msg::frame(&done));
    out
}

/// Connect, answering the want_config request with `handshake_stream`.
async fn connect() -> (Meshtastic<DuplexStream>, DuplexStream) {
    let (host, mut node) = duplex(1 << 16);
    let node_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let id = loop {
            let mut tmp = [0u8; 256];
            let n = node.read(&mut tmp).await.unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(m) = msg::take_frame(&mut buf) {
                let f = fields(&m).next().unwrap().unwrap();
                assert_eq!(f.num, 3, "want_config_id");
                break f.value.u32().unwrap();
            }
        };
        node.write_all(&handshake_stream(id)).await.unwrap();
        node
    });
    let mesh = Meshtastic::with_io(host).await.unwrap();
    (mesh, node_task.await.unwrap())
}

/// The next ToRadio the node received.
async fn next_to_radio(node: &mut DuplexStream) -> Vec<u8> {
    let mut buf = Vec::new();
    loop {
        if let Some(m) = msg::take_frame(&mut buf) {
            return m;
        }
        let mut tmp = [0u8; 256];
        let n = node.read(&mut tmp).await.unwrap();
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// `ToRadio.packet` -> (to, portnum, payload, hop_limit).
fn unpack(to_radio: &[u8]) -> (u32, u32, Vec<u8>, u32) {
    let f = fields(to_radio).next().unwrap().unwrap();
    assert_eq!(f.num, 1, "ToRadio.packet");
    let pkt = f.value.bytes().unwrap();
    let (mut to, mut hop, mut data) = (0, 0, None);
    for f in fields(pkt) {
        let f = f.unwrap();
        match f.num {
            2 => to = f.value.fixed32().unwrap(),
            4 => data = Some(f.value.bytes().unwrap().to_vec()),
            9 => hop = f.value.u32().unwrap(),
            n => panic!("unexpected MeshPacket field {n}"),
        }
    }
    assert!(data.is_some());
    let p = msg::MeshPacket::decode(pkt).unwrap();
    let (port, payload) = p.decoded.unwrap();
    (to, port, payload, hop)
}

fn packet(from: u32, port: u32, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    put_uint(&mut data, 1, u64::from(port));
    put_bytes(&mut data, 2, payload);
    let mut p = Vec::new();
    put_fixed32(&mut p, 1, from);
    put_fixed32(&mut p, 2, msg::BROADCAST);
    put_bytes(&mut p, 4, &data);
    put_float(&mut p, 8, 6.5);
    put_int32(&mut p, 12, -101);
    from_radio(2, &p)
}

fn telemetry(battery: u32, air_util_tx: f32) -> Vec<u8> {
    let mut dm = Vec::new();
    put_uint(&mut dm, 1, u64::from(battery));
    put_float(&mut dm, 4, air_util_tx);
    let mut t = Vec::new();
    put_fixed32(&mut t, 1, 1_700_000_000);
    put_bytes(&mut t, 2, &dm);
    t
}

fn tx(f: Frame) -> TxFrame {
    TxFrame {
        channel_mhz: 869.525,
        power_dbm: 14,
        bytes: f.encode().unwrap(),
    }
}

#[tokio::test]
async fn handshake_reads_node_hardware_and_modem() {
    let (mesh, _node) = connect().await;
    assert_eq!(mesh.node_num(), ME);
    assert_eq!(mesh.hw_model(), Some(43));
    assert_eq!(mesh.caps(), &ModuleCaps::meshtastic_863_928());
    let m = mesh.modem();
    assert_eq!((m.sf, m.bw_khz, m.cr, m.preamble), (9, 250, 1, 16));
    assert!(m.crc && !m.implicit_header);
    assert_eq!(mesh.airtime_factor(), 1);
}

#[test]
fn modem_follows_presets_and_explicit_settings() {
    let preset = |p| {
        modem_of(&LoraSettings {
            use_preset: true,
            modem_preset: p,
            ..Default::default()
        })
    };
    assert_eq!((preset(0).sf, preset(0).bw_khz, preset(0).cr), (11, 250, 1));
    assert_eq!((preset(1).sf, preset(1).bw_khz, preset(1).cr), (12, 125, 4));
    assert_eq!((preset(7).sf, preset(7).bw_khz, preset(7).cr), (11, 125, 4));
    assert_eq!((preset(6).sf, preset(5).sf, preset(3).sf), (7, 8, 10));
    let custom = modem_of(&LoraSettings {
        spread_factor: 10,
        bandwidth: 62,
        coding_rate: 7,
        ..Default::default()
    });
    assert_eq!((custom.sf, custom.bw_khz, custom.cr), (10, 62, 3));
}

#[tokio::test]
async fn beacon_becomes_a_broadcast_position() {
    let (mut mesh, mut node) = connect().await;
    let beacon = Beacon {
        position: Some((12.971_6, 77.594_6)),
        alt_m: Some(-12.0),
        battery_pct: Some(80),
        mode: "AUTO".into(),
    };
    let f = Frame {
        node_id: 3,
        kind: Kind::Beacon,
        payload: beacon.to_payload(),
    };
    mesh.send(&tx(f)).await.unwrap();
    let (to, port, payload, hop) = unpack(&next_to_radio(&mut node).await);
    assert_eq!((to, port, hop), (msg::BROADCAST, port::POSITION_APP, 5));
    let pos = Position::decode(&payload).unwrap();
    assert_eq!(pos.latitude_i, Some(129_716_000));
    assert_eq!(pos.longitude_i, Some(775_946_000));
    assert_eq!(pos.altitude, Some(-12));
    assert_eq!(Position::decode(&pos.encode()).unwrap(), pos);
}

#[tokio::test]
async fn text_is_forwarded_and_status_is_not() {
    let (mut mesh, mut node) = connect().await;
    let status = Frame {
        node_id: 3,
        kind: Kind::Status,
        payload: Status {
            about_node: 1,
            rssi_dbm: -90,
            snr_db: 2.0,
        }
        .to_payload(),
    };
    mesh.send(&tx(status)).await.unwrap();
    let text = Frame {
        node_id: 3,
        kind: Kind::Text,
        payload: b"hello mesh".to_vec(),
    };
    mesh.send(&tx(text)).await.unwrap();
    let (_, port, payload, _) = unpack(&next_to_radio(&mut node).await);
    assert_eq!(
        (port, payload.as_slice()),
        (port::TEXT_MESSAGE_APP, &b"hello mesh"[..])
    );
}

#[tokio::test]
async fn positions_from_other_nodes_carry_their_battery() {
    let (mut mesh, mut node) = connect().await;
    let mut wire = packet(OTHER, port::TELEMETRY_APP, &telemetry(64, 1.0));
    let own_position = Position {
        latitude_i: Some(1),
        longitude_i: Some(1),
        altitude: None,
    };
    wire.extend(packet(ME, port::POSITION_APP, &own_position.encode()));
    wire.extend(packet(OTHER, 1, b"chat"));
    let pos = Position {
        latitude_i: Some(-338_688_000),
        longitude_i: Some(1_512_093_000),
        altitude: Some(40),
    };
    wire.extend(packet(OTHER, port::POSITION_APP, &pos.encode()));
    node.write_all(&wire).await.unwrap();
    match mesh.recv().await.unwrap() {
        RxEvent::MeshPosition {
            node_id,
            lat_deg,
            lon_deg,
            alt_m,
            battery_pct,
            rssi_dbm,
            snr_db,
        } => {
            assert_eq!(node_id, OTHER);
            assert!((lat_deg + 33.8688).abs() < 1e-9 && (lon_deg - 151.2093).abs() < 1e-9);
            assert_eq!(alt_m, Some(40.0));
            assert_eq!(battery_pct, Some(64));
            assert_eq!(rssi_dbm, Some(-101));
            assert_eq!(snr_db, Some(6.5));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(mesh.reported_air_util_pct(), None);
}

#[tokio::test]
async fn own_device_metrics_report_air_util_tx() {
    let (mut mesh, mut node) = connect().await;
    let mut wire = packet(ME, port::TELEMETRY_APP, &telemetry(101, 3.5));
    let pos = Position {
        latitude_i: Some(10),
        longitude_i: Some(20),
        altitude: None,
    };
    wire.extend(packet(OTHER, port::POSITION_APP, &pos.encode()));
    node.write_all(&wire).await.unwrap();
    assert!(matches!(
        mesh.recv().await.unwrap(),
        RxEvent::MeshPosition { .. }
    ));
    assert_eq!(mesh.reported_air_util_pct(), Some(3.5));
}

#[tokio::test]
async fn apply_plan_sets_region_and_power_on_the_local_node() {
    let (mut mesh, mut node) = connect().await;
    mesh.apply_plan(Some("IN"), 30).await.unwrap();
    let (to, port, admin, _) = unpack(&next_to_radio(&mut node).await);
    assert_eq!((to, port), (ME, port::ADMIN_APP));
    let set_config = fields(&admin).next().unwrap().unwrap();
    assert_eq!(set_config.num, 34);
    let lora_field = fields(set_config.value.bytes().unwrap())
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(lora_field.num, 6);
    let lora = lora_field.value.bytes().unwrap();
    let s = LoraSettings::decode(lora).unwrap();
    assert_eq!((s.region, s.modem_preset, s.hop_limit), (10, 4, 5));
    let mut by_num = HashMap::new();
    for f in fields(lora) {
        let f = f.unwrap();
        by_num.insert(f.num, f.value);
    }
    assert_eq!(by_num[&msg::lora::TX_POWER].i32(), Some(22));
    assert_eq!(by_num[&msg::lora::TX_ENABLED].bool(), Some(true));
    assert_eq!(by_num[&11].u32(), Some(20));
    assert!(!by_num.contains_key(&msg::lora::OVERRIDE_DUTY_CYCLE));

    mesh.apply_plan(None, 0).await.unwrap();
    let (_, _, admin, _) = unpack(&next_to_radio(&mut node).await);
    let set_config = fields(&admin).next().unwrap().unwrap();
    let lora = fields(set_config.value.bytes().unwrap())
        .next()
        .unwrap()
        .unwrap();
    let mut by_num = HashMap::new();
    for f in fields(lora.value.bytes().unwrap()) {
        let f = f.unwrap();
        by_num.insert(f.num, f.value);
    }
    assert_eq!(by_num[&msg::lora::REGION].u32(), Some(10));
    assert_eq!(by_num[&msg::lora::TX_ENABLED].bool(), Some(false));
    assert_eq!(by_num[&msg::lora::TX_POWER].i32(), Some(1));

    let err = mesh.apply_plan(Some("XX"), 10).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn region_names_map_to_region_codes() {
    let want = [
        ("IN", 10),
        ("EU_868", 3),
        ("EU_433", 2),
        ("RU", 9),
        ("US", 1),
        ("ANZ", 6),
        ("ANZ_433", 22),
        ("BR_902", 26),
        ("JP", 5),
        ("KR", 7),
        ("TW", 8),
        ("MY_919", 17),
        ("TH", 12),
    ];
    for (name, code) in want {
        assert_eq!(region_code(name), Some(code), "{name}");
    }
    assert_eq!(region_code("EU"), None);
}

#[test]
fn stream_framing_skips_console_text_and_bad_lengths() {
    let mut buf = b"log \x94\x00".to_vec();
    buf.extend_from_slice(&[0x94, 0xC3, 0xFF, 0xFF]);
    buf.extend(msg::frame(&[8, 1]));
    buf.extend_from_slice(&[0x94, 0xC3, 0x00]);
    assert_eq!(msg::take_frame(&mut buf), Some(vec![8, 1]));
    assert_eq!(msg::take_frame(&mut buf), None);
    assert_eq!(buf, [0x94, 0xC3, 0x00]);
}

#[test]
fn from_radio_round_trips() {
    let mut node = Vec::new();
    put_uint(&mut node, 1, u64::from(OTHER));
    let mut user = Vec::new();
    put_uint(&mut user, 5, 9);
    put_bytes(&mut node, 2, &user);
    let mut dm = Vec::new();
    put_uint(&mut dm, 1, 77);
    put_bytes(&mut node, 6, &dm);
    let mut b = from_radio(4, &node);
    let m = msg::take_frame(&mut b).unwrap();
    assert_eq!(
        FromRadio::decode(&m).unwrap(),
        FromRadio::NodeInfo {
            num: OTHER,
            hw_model: Some(9),
            metrics: Some(msg::DeviceMetrics {
                battery_level: Some(77),
                air_util_tx: None,
            }),
        }
    );
    let mut reb = Vec::new();
    put_uint(&mut reb, 8, 1);
    assert_eq!(FromRadio::decode(&reb).unwrap(), FromRadio::Rebooted);
    let out = OutPacket {
        to: 7,
        portnum: port::TEXT_MESSAGE_APP,
        payload: b"x".to_vec(),
        hop_limit: 3,
    };
    assert_eq!(
        unpack(&msg::to_radio_packet(&out)),
        (7, 1, b"x".to_vec(), 3)
    );
    let mut want = Vec::new();
    put_uint(&mut want, 3, 42);
    assert_eq!(msg::want_config(42), want);
}

#[tokio::test]
async fn a_beacon_is_charged_on_the_packet_the_node_radiates() {
    let (mesh, _node) = connect().await;
    let beacon = Frame {
        node_id: 1,
        kind: Kind::Beacon,
        payload: Beacon {
            position: Some((12.971_6, 77.594_6)),
            alt_m: Some(920.0),
            battery_pct: Some(81),
            mode: "AUTO".into(),
        }
        .to_payload(),
    }
    .encode()
    .unwrap();
    // 16 header + Data{portnum 2, payload 2 + Position 13} + 8 firmware reserve.
    assert_eq!(mesh.on_air_len(&beacon).unwrap(), Some(41));
    assert!(mesh.on_air_len(&beacon).unwrap().unwrap() > beacon.len());
    let status = Frame {
        node_id: 1,
        kind: Kind::Status,
        payload: Status {
            about_node: 2,
            rssi_dbm: -90,
            snr_db: 3.0,
        }
        .to_payload(),
    }
    .encode()
    .unwrap();
    assert_eq!(mesh.on_air_len(&status).unwrap(), None);
}
