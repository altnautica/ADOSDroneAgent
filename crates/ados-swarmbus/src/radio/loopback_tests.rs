use super::*;
use crate::frame::{build_frame, FRAME_HEADER_LEN};
use std::collections::HashMap;

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

fn cfg(node: &str) -> LoopbackConfig {
    LoopbackConfig::from_lookup(lookup(&[(ENV_NODE, node)])).unwrap()
}

#[test]
fn defaults_point_at_a_local_relay_on_the_standard_base() {
    let c = cfg("gs-swarm");
    assert_eq!(c.relay_host, DEFAULT_RELAY_HOST);
    assert_eq!(c.relay_port, 7900);
    assert_eq!(c.node_addr, None);
    assert_eq!(c.listen, "0.0.0.0:0".parse::<SocketAddr>().unwrap());
}

#[test]
fn every_setting_is_read_from_its_variable() {
    let c = LoopbackConfig::from_lookup(lookup(&[
        (ENV_NODE, "drone-2-swarm"),
        (ENV_RELAY_HOST, "relay.example.com"),
        (ENV_RELAY_PORT, "15800"),
        (ENV_NODE_ADDR, "192.168.1.50"),
        (ENV_LISTEN, "127.0.0.1:21500"),
    ]))
    .unwrap();
    assert_eq!(c.relay_host, "relay.example.com");
    assert_eq!(c.relay_port, 15800);
    assert_eq!(c.node_addr.as_deref(), Some("192.168.1.50"));
    assert_eq!(c.listen.port(), 21500);
}

#[test]
fn a_bad_or_missing_setting_is_refused_by_name() {
    let cases: [(&[(&str, &str)], &str); 5] = [
        (&[], ENV_NODE),
        (&[(ENV_NODE, "  ")], ENV_NODE),
        (
            &[(ENV_NODE, "n"), (ENV_RELAY_PORT, "seventy")],
            ENV_RELAY_PORT,
        ),
        (
            &[(ENV_NODE, "n"), (ENV_RELAY_PORT, "65535")],
            ENV_RELAY_PORT,
        ),
        (&[(ENV_NODE, "n"), (ENV_LISTEN, "localhost")], ENV_LISTEN),
    ];
    for (pairs, var) in cases {
        let err = LoopbackConfig::from_lookup(lookup(pairs)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains(var), "{var}: {err}");
    }
    let long = "n".repeat(MAX_NODE_LEN + 1);
    let err = LoopbackConfig::from_lookup(lookup(&[(ENV_NODE, &long)])).unwrap_err();
    assert!(err.to_string().contains(ENV_NODE), "{err}");
}

/// The uplink datagram is the relay's `!I channel, !B name_len, name` head
/// followed by the unmodified injected frame.
#[test]
fn the_uplink_datagram_carries_the_injected_frame_byte_for_byte() {
    let frame = build_frame(1, 5, &[0x5A; 50]);
    let prefix = uplink_prefix("drone-1-swarm");
    let mut out = [0u8; MAX_UPLINK_LEN];
    let n = encode_uplink(&prefix, &frame, &mut out).unwrap();
    let dg = &out[..n];
    assert_eq!(&dg[..4], &[0x00, 0x00, 0xAD, 0x03], "channel, big-endian");
    assert_eq!(dg[4] as usize, "drone-1-swarm".len());
    assert_eq!(&dg[5..18], b"drone-1-swarm");
    assert_eq!(&dg[18..], &frame[..]);
    assert_eq!(frame.len(), FRAME_HEADER_LEN + 50);
}

#[test]
fn an_oversize_frame_is_refused_before_it_is_sent() {
    let prefix = uplink_prefix("n");
    let mut out = [0u8; MAX_UPLINK_LEN];
    let big = vec![0u8; MAX_FRAME_LEN + 1];
    let err = encode_uplink(&prefix, &big, &mut out).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn only_downlinks_on_the_swarm_channel_are_read() {
    let mut dg = LOOPBACK_CHANNEL.to_be_bytes().to_vec();
    dg.extend_from_slice(b"frame");
    assert_eq!(downlink_frame(&dg), Some(&b"frame"[..]));
    let mut other = 0x0001_0100u32.to_be_bytes().to_vec();
    other.extend_from_slice(b"frame");
    assert_eq!(downlink_frame(&other), None, "a wfb lane's channel");
    assert_eq!(downlink_frame(&dg[..3]), None, "shorter than the channel");
}

/// The swarm channel decodes to a slot no fleet can hold, so no wfb lane on
/// the relay shares it.
#[test]
fn the_swarm_channel_cannot_collide_with_a_wfb_lane() {
    let link_id = LOOPBACK_CHANNEL >> 8;
    let slot = link_id & 0xFF;
    assert!(slot > u32::from(ados_radio::config::FLEET_MAX_SLOTS));
}

#[test]
fn the_register_line_is_one_json_object_per_direction() {
    let mut c = cfg("drone-1-swarm");
    let line = register_line(&c, "rx", 21000);
    assert!(line.ends_with('\n'));
    let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"op": "register", "node": "drone-1-swarm",
            "channel": LOOPBACK_CHANNEL, "direction": "rx", "port": 21000})
    );
    c.node_addr = Some("192.168.1.50".into());
    let v: serde_json::Value = serde_json::from_str(&register_line(&c, "tx", 1)).unwrap();
    assert_eq!(v["addr"], "192.168.1.50");
    assert_eq!(v["direction"], "tx");
}

/// An unreachable relay fails the open with the control address in the
/// message, so the service's retry log says what it is waiting for.
#[tokio::test]
async fn an_unreachable_relay_fails_the_open_and_names_it() {
    // Bind and drop a listener to find a port nothing is serving.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let c = LoopbackConfig::from_lookup(lookup(&[
        (ENV_NODE, "n"),
        (ENV_RELAY_PORT, &port.to_string()),
        (ENV_LISTEN, "127.0.0.1:0"),
    ]))
    .unwrap();
    let Err(err) = Radio::open(&c, 1) else {
        panic!("no relay is listening");
    };
    assert!(err.to_string().contains(&port.to_string()), "{err}");
}
