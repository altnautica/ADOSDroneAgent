# ados-protocol

[![crates.io](https://img.shields.io/crates/v/ados-protocol.svg)](https://crates.io/crates/ados-protocol)
[![docs.rs](https://img.shields.io/docsrs/ados-protocol)](https://docs.rs/ados-protocol)
[![license](https://img.shields.io/crates/l/ados-protocol.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Wire contracts for the ADOS Drone Agent: length-prefixed IPC framing, plugin RPC envelope and capability tokens, the vision frame bus, MSP and MAVLink helpers.**

The ADOS Drone Agent is a set of cooperating processes that talk over a small number of fixed wire formats. This crate implements each of those formats once, so any Rust program (a plugin, a tool, a test harness or another service) speaks byte-for-byte the same wire as the agent. Reach for it when you need to read or write agent IPC frames directly; to write a plugin, start with [`ados-sdk`](https://crates.io/crates/ados-sdk), which builds on it.

```rust
use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};

let request = Envelope {
    version: PROTOCOL_VERSION,
    kind: "request".into(),
    method: "ping".into(),
    capability: String::new(),
    args: rmpv::Value::Map(vec![]),
    request_id: "r1".into(),
    token: String::new(),
    error: None,
};

// A complete frame: 4-byte big-endian length, then the msgpack body.
let wire = request.encode_frame().unwrap();
let header: [u8; HEADER_SIZE] = wire[..HEADER_SIZE].try_into().unwrap();
let len = decode_len(header, PLUGIN_MAX_FRAME, true).unwrap();
assert_eq!(len, wire.len() - HEADER_SIZE);

let decoded = Envelope::from_msgpack(&wire[HEADER_SIZE..]).unwrap();
assert_eq!(decoded, request);
```

## Highlights

- One implementation of every agent wire contract, shared by the agent's own Rust services and by third-party plugins.
- Frame length limits are enforced on both encode and decode, with a per-contract zero-length policy.
- Capability tokens are HMAC-SHA256 signed and verified in constant time, with expiry checks.
- Interoperability tests decode fixtures produced by the agent's Python side, so both languages stay on the same bytes.
- Optional features keep the default build small: the MAVLink dialect, a `tracing` log shipping layer and a TLS crypto provider are opt-in.

## How it works

The crate is a collection of modules, one per wire seam:

- [`frame`](https://docs.rs/ados-protocol/latest/ados_protocol/frame/): 4-byte big-endian length-prefixed framing, shared by the MAVLink socket, the vehicle-state socket and the plugin RPC socket.
- [`plugin`](https://docs.rs/ados-protocol/latest/ados_protocol/plugin/): the plugin RPC envelope (a msgpack map with short keys `v`, `t`, `m`, `c`, `a`, `id`, `tok`, `err`) and the pipe-delimited HMAC-SHA256 capability token bound to a plugin id, a granted capability set, a session and an expiry.
- [`framebus`](https://docs.rs/ados-protocol/latest/ados_protocol/framebus/): the vision frame transport, meaning the frame descriptor shape and the shared-memory ring layout frames travel in.
- [`state`](https://docs.rs/ados-protocol/latest/ados_protocol/state/): the vehicle-state codec, with a newline-delimited JSON reader and a length-prefixed msgpack codec.
- [`logd`](https://docs.rs/ados-protocol/latest/ados_protocol/logd/): logging and telemetry contracts: versioned ingest frames (logs, telemetry, events, hardware snapshots), the read API envelope and secret-field redaction.
- [`capabilities`](https://docs.rs/ados-protocol/latest/ados_protocol/capabilities/) and [`dispatch`](https://docs.rs/ados-protocol/latest/ados_protocol/dispatch/): the generated capability catalog and the table mapping each plugin RPC method to the capability it requires.
- [`contracts`](https://docs.rs/ados-protocol/latest/ados_protocol/contracts/) and [`paths`](https://docs.rs/ados-protocol/latest/ados_protocol/paths/): the generated registry of contract version numbers, runtime directory, socket and state file names, and each state file's freshness window.
- [`sidecar`](https://docs.rs/ados-protocol/latest/ados_protocol/sidecar/): helpers for on-disk state files: schema version check, atomic writers and a freshness-gated reader.
- [`pairing_posture`](https://docs.rs/ados-protocol/latest/ados_protocol/pairing_posture/) and [`dashboard_session`](https://docs.rs/ados-protocol/latest/ados_protocol/dashboard_session/): data-plane authentication primitives (constant-time key comparison, loopback trust, access decision) and self-contained HMAC session tokens.
- [`mavlink_ingest`](https://docs.rs/ados-protocol/latest/ados_protocol/mavlink_ingest/): the client for a node's MAVLink republish seam, through which frames received off-board enter the node's own frame fan-out.
- [`flight_modes`](https://docs.rs/ados-protocol/latest/ados_protocol/flight_modes/): ArduPilot flight-mode tables keyed by vehicle type.
- [`msp`](https://docs.rs/ados-protocol/latest/ados_protocol/msp/) and [`mavlink`](https://docs.rs/ados-protocol/latest/ados_protocol/mavlink/): MSP helpers and, with the `mavlink` feature, the MAVLink codec for the ardupilotmega dialect.
- [`shutdown`](https://docs.rs/ados-protocol/latest/ados_protocol/shutdown/): a latching shutdown signal, so a loop that was busy when the stop arrived still sees it.

The plugin RPC wire is a sequence of frames over a Unix domain socket. Each frame carries one [`Envelope`](https://docs.rs/ados-protocol/latest/ados_protocol/plugin/struct.Envelope.html); the capability token travels in every envelope and the host verifies it before routing the request.

## Cargo features

|Feature|Default|What it enables|
|---|---|---|
|`mavlink`|no|The MAVLink codec module for the ardupilotmega dialect|
|`tracing-layer`|no|A `tracing` layer that ships log events to the agent's log ingest socket|
|`tls`|no|A process-default rustls crypto provider for crates that make TLS connections|

## Versioning

This crate is versioned in lockstep with the ADOS Drone Agent: version X.Y.Z speaks the wire of agent X.Y.Z.

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.
[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
