//! Wire vectors for ground-side ports of the aux-lane encoders.
//!
//! A ground station that is not this agent (the Android app's phone radio)
//! speaks the same aux framing, relay tickets, relay-proxy fragments and link
//! feedback. These tests produce each vector with this crate's own encoders
//! and print it, so the other implementation's tests can assert identical
//! bytes. Run with `cargo test -p ados-protocol ground_vectors -- --nocapture`.

use crate::aux_mux::{self, AuxChannel};
use crate::aux_rpc::{
    decode_request_fragment, decode_response, encode_request, encode_response_fragment,
    pack_response, split_response, RequestParts, RpcMethod,
};
use crate::link_feedback::LinkFeedback;
use crate::relay_ticket::{
    tunnel_binding_body, RelayTicketIssuer, RequestBinding, TUNNEL_BINDING_METHOD,
    TUNNEL_BINDING_PATH,
};

/// The per-pair secret, as the 64-hex text both ends hold.
const SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const DRONE: &str = "drone-1";
const NOW: i64 = 1_700_000_000;
const NONCE: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

fn show(name: &str, bytes: &[u8]) {
    println!("VECTOR {name} {}", hex::encode(bytes));
}

/// The response body pattern the ground-side decoder test regenerates.
fn response_pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 3) & 0xFF) as u8).collect()
}

/// The request body pattern the ground-side encoder test regenerates.
fn request_pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn ticket(method: &str, path: &str, body: &[u8]) -> String {
    RelayTicketIssuer::from_secret(SECRET.as_bytes()).mint_with_nonce(
        DRONE,
        &RequestBinding::new(method, path, body),
        NOW,
        NONCE,
    )
}

#[test]
fn ground_vectors_aux_frames() {
    show(
        "aux_mavlink",
        &aux_mux::encode(AuxChannel::Mavlink, b"payload bytes").unwrap(),
    );
    show(
        "aux_pilot",
        &aux_mux::encode(AuxChannel::Pilot, &[7, 0x81, 0xA1]).unwrap(),
    );
    show(
        "aux_empty_request",
        &aux_mux::encode(AuxChannel::Request, &[]).unwrap(),
    );
}

#[test]
fn ground_vectors_relay_tickets() {
    let put_body = br#"{"key":"video.wfb.fleet_slot","value":3}"#;
    let op = serde_json::json!({"op": "put", "key": "video.wfb.bitrate", "value": "8000"});
    let tunnel = tunnel_binding_body(&op);
    let get_op = serde_json::json!({"op": "get", "key": "radio"});
    show("tunnel_binding_put", &tunnel);
    show("tunnel_binding_get", &tunnel_binding_body(&get_op));
    for (name, method, path, body) in [
        ("ticket_get_status", "GET", "/api/status", &b""[..]),
        ("ticket_put_config", "PUT", "/api/config", &put_body[..]),
        (
            "ticket_tunnel",
            TUNNEL_BINDING_METHOD,
            TUNNEL_BINDING_PATH,
            &tunnel[..],
        ),
    ] {
        let t = ticket(method, path, body);
        let issuer = RelayTicketIssuer::from_secret(SECRET.as_bytes());
        issuer
            .verify(&t, DRONE, &RequestBinding::new(method, path, body), NOW)
            .expect("the printed ticket verifies");
        println!("TICKET {name} {t}");
    }
}

#[test]
fn ground_vectors_link_feedback() {
    let measured = LinkFeedback {
        loss_percent: 5.25,
        rssi_dbm: -61.4,
        snr_db: 12.4,
        packets_received: 485,
        fec_failed: 3,
        bitrate_kbps: 7_850,
        has_measurement: true,
        target_slot: 2,
    };
    let deaf = LinkFeedback {
        loss_percent: 0.0,
        rssi_dbm: -100.0,
        snr_db: -3.0,
        packets_received: 0,
        fec_failed: 0,
        bitrate_kbps: 0,
        has_measurement: false,
        target_slot: 1,
    };
    show("feedback_measured", &measured.encode());
    show("feedback_deaf", &deaf.encode());
}

#[test]
fn ground_vectors_rpc_requests() {
    let body = br#"{"key":"video.wfb.fleet_slot","value":3}"#;
    let t = ticket("PUT", "/api/config", body);
    let frags = encode_request(
        RpcMethod::Put,
        0x0102_0304,
        DRONE.as_bytes(),
        &RequestParts {
            path: b"/api/config",
            content_type: b"application/json",
            ticket: t.as_bytes(),
            body,
        },
    )
    .unwrap();
    assert_eq!(frags.len(), 1);
    show("rpc_request_put", &frags[0]);

    let big = request_pattern(2600);
    let frags = encode_request(
        RpcMethod::Post,
        7,
        DRONE.as_bytes(),
        &RequestParts {
            path: b"/api/mission",
            content_type: b"application/octet-stream",
            ticket: b"",
            body: &big,
        },
    )
    .unwrap();
    assert_eq!(frags.len(), 3);
    for (i, f) in frags.iter().enumerate() {
        decode_request_fragment(f).unwrap();
        show(&format!("rpc_request_big_{i}"), f);
    }
}

/// One response vector: name, HTTP status, body, headers.
type ResponseCase = (&'static str, u16, Vec<u8>, Vec<(String, String)>);

#[test]
fn ground_vectors_rpc_responses() {
    let cases: [ResponseCase; 3] = [
        ("small", 200, br#"{"ok":true}"#.to_vec(), vec![]),
        ("empty", 204, Vec::new(), vec![]),
        (
            "big",
            200,
            response_pattern(2500),
            vec![("content-type".into(), "application/octet-stream".into())],
        ),
    ];
    for (name, status, body, headers) in cases {
        let (wire_status, object) = pack_response(status, &body, &headers);
        let split = split_response(&object).unwrap();
        let total = split.symbols.len() as u16;
        for (i, symbol) in split.symbols.iter().enumerate() {
            let payload = encode_response_fragment(
                DRONE.as_bytes(),
                0x0A0B_0C0D,
                wire_status,
                i as u16,
                total,
                split.oti,
                symbol,
            )
            .unwrap();
            decode_response(&payload).unwrap();
            show(&format!("rpc_response_{name}_{i}"), &payload);
        }
    }
}
