//! Cross-language wire-compatibility tests.
//!
//! `tests/interop/fixtures.json` is generated from the live Python agent code
//! (`tests/interop/generate_fixtures.py` imports `ados.plugins.rpc`). These
//! tests assert the Rust `ados-protocol` crate produces and consumes the exact
//! same bytes, which is the regression guard for the frozen wire contracts.
//!
//! Regenerate the fixture with:
//!   .venv/bin/python crates/ados-protocol/tests/interop/generate_fixtures.py

use std::collections::BTreeSet;

use ados_protocol::frame;
use ados_protocol::plugin::{CapabilityToken, Envelope, TokenIssuer};
use ados_protocol::state;
use serde_json::Value;

fn fixtures() -> Value {
    let raw = include_str!("interop/fixtures.json");
    serde_json::from_str(raw).expect("fixtures.json parses")
}

#[test]
fn capability_token_matches_python() {
    let f = fixtures();
    let t = &f["token"];

    let secret = hex::decode(t["secret_hex"].as_str().unwrap()).unwrap();
    let issuer = TokenIssuer::new(secret);

    let caps: BTreeSet<String> = t["granted_caps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    let issued_at = t["issued_at"].as_i64().unwrap();
    let ttl = t["ttl"].as_i64().unwrap();
    let session_id = t["session_id"].as_str().unwrap();
    let plugin_id = t["plugin_id"].as_str().unwrap();

    let minted = issuer.mint_at(plugin_id, &caps, ttl, issued_at, session_id);

    // The HMAC signature must match the one the Python issuer produced.
    assert_eq!(minted.signature, t["signature"].as_str().unwrap());
    // And the full pipe-delimited string form must be byte-identical.
    assert_eq!(
        minted.to_token_string(),
        t["token_string"].as_str().unwrap()
    );

    // Parsing the Python-produced string back must reconstruct the same token,
    // and verification against the same secret must pass.
    let parsed = CapabilityToken::from_token_string(t["token_string"].as_str().unwrap()).unwrap();
    assert_eq!(parsed, minted);
    assert!(issuer.verify(&parsed, issued_at + 1).is_ok());
}

#[test]
fn plugin_envelope_frame_is_byte_identical_to_python() {
    let f = fixtures();
    let e = &f["envelope"];

    // Build the envelope with the same args key order the generator used
    // ({"topic": "demo", "n": 7}) so the msgpack map bytes line up.
    let env = Envelope {
        version: e["version"].as_i64().unwrap(),
        kind: e["type"].as_str().unwrap().to_string(),
        method: e["method"].as_str().unwrap().to_string(),
        capability: e["capability"].as_str().unwrap().to_string(),
        args: rmpv::Value::Map(vec![
            (rmpv::Value::from("topic"), rmpv::Value::from("demo")),
            (rmpv::Value::from("n"), rmpv::Value::from(7i64)),
        ]),
        request_id: e["request_id"].as_str().unwrap().to_string(),
        token: e["token"].as_str().unwrap().to_string(),
        error: None,
    };

    assert_eq!(
        hex::encode(env.to_msgpack().unwrap()),
        e["body_hex"].as_str().unwrap()
    );
    assert_eq!(
        hex::encode(env.encode_frame().unwrap()),
        e["frame_hex"].as_str().unwrap()
    );
}

#[test]
fn plugin_envelope_decodes_python_bytes() {
    let f = fixtures();
    let e = &f["envelope"];

    let frame_bytes = hex::decode(e["frame_hex"].as_str().unwrap()).unwrap();
    // Strip the 4-byte length prefix and decode the msgpack body.
    let header: [u8; 4] = frame_bytes[..4].try_into().unwrap();
    let len = frame::decode_len(header, frame::PLUGIN_MAX_FRAME, true).unwrap();
    let body = &frame_bytes[4..4 + len];
    let env = Envelope::from_msgpack(body).unwrap();

    assert_eq!(env.kind, "request");
    assert_eq!(env.method, "event.publish");
    assert_eq!(env.capability, "event.publish");
    assert_eq!(env.request_id, "req-001");
    assert_eq!(env.version, 1);
    assert_eq!(env.error, None);
}

#[test]
fn state_v1_decodes_python_bytes() {
    let f = fixtures();
    let s = &f["state_v1"];

    let wire = hex::decode(s["wire_hex"].as_str().unwrap()).unwrap();
    let decoded = state::decode_v1_line(&wire).unwrap();
    // Semantic equality with the source state (Python json.dumps uses ", "
    // separators; serde_json is compact, so byte-equality on re-encode is not
    // expected. The v1 reader only needs to consume Python output; v2 msgpack
    // is the canonical forward wire.)
    assert_eq!(decoded, s["state"]);
}

#[test]
fn state_v2_decodes_python_msgpack() {
    let f = fixtures();
    let s = &f["state_v2"];

    // The Python agent's v2 wire body is msgpack(state); the Rust state hub
    // must decode it to the same snapshot. (body_hex is the frame payload
    // without the 4-byte length prefix.)
    let body = hex::decode(s["body_hex"].as_str().unwrap()).unwrap();
    let decoded = state::decode_v2(&body).unwrap();
    assert_eq!(decoded, s["state"]);

    // And a frame the Rust side encodes round-trips back to the same state,
    // so a Python v2 consumer reading Rust output sees identical fields.
    let frame = state::encode_v2(&decoded).unwrap();
    let reparsed = state::decode_v2(&frame[frame::HEADER_SIZE..]).unwrap();
    assert_eq!(reparsed, s["state"]);
}
