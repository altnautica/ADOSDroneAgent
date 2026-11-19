//! Wire vectors for ground-side ports of the config tunnel.
//!
//! Prints the TUNNEL frames this crate's framing produces for one ticketed
//! request and one multi-chunk response, so another ground implementation
//! can assert identical bytes. Run with
//! `cargo test -p ados-config-tunnel ground_vectors -- --nocapture`.

use ados_protocol::config_tunnel::CONFIG_TUNNEL_PAYLOAD_TYPE;
use ados_protocol::mavlink::{build_tunnel_v2, MavHeader};
use ados_protocol::relay_ticket::{
    tunnel_binding_body, RelayTicketIssuer, RequestBinding, TUNNEL_BINDING_METHOD,
    TUNNEL_BINDING_PATH,
};

use crate::{FRAMING, MAX_CHUNKS};

const SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn frames(request_id: u32, is_response: bool, body: &[u8]) -> Vec<Vec<u8>> {
    FRAMING
        .chunk_message(request_id, is_response, false, body, MAX_CHUNKS)
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let header = MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: i as u8,
            };
            build_tunnel_v2(header, CONFIG_TUNNEL_PAYLOAD_TYPE, 0, 0, chunk).unwrap()
        })
        .collect()
}

#[test]
fn ground_vectors_config_tunnel() {
    let mut op = serde_json::json!({"op": "put", "key": "video.wfb.bitrate", "value": "8000"});
    let binding = tunnel_binding_body(&op);
    let ticket = RelayTicketIssuer::from_secret(SECRET.as_bytes()).mint_with_nonce(
        "drone-1",
        &RequestBinding::new(TUNNEL_BINDING_METHOD, TUNNEL_BINDING_PATH, &binding),
        1_700_000_000,
        "0f1e2d3c4b5a69788796a5b4c3d2e1f0",
    );
    op["ticket"] = serde_json::json!(ticket);
    let body = serde_json::to_vec(&op).unwrap();
    println!("VECTOR tunnel_request_body {}", hex(&body));
    for (i, f) in frames(5, false, &body).iter().enumerate() {
        println!("VECTOR tunnel_request_{i} {}", hex(f));
    }
    let response: Vec<u8> = format!(r#"{{"blob":"{}"}}"#, "z".repeat(300)).into_bytes();
    for (i, f) in frames(5, true, &response).iter().enumerate() {
        println!("VECTOR tunnel_response_{i} {}", hex(f));
    }
}
