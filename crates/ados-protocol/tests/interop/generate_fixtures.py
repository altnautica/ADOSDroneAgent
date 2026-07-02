#!/usr/bin/env python3
"""Generate cross-language wire fixtures from the real agent code.

This imports the live ``ados.plugins.rpc`` module (and reproduces the
``core/ipc.py`` state framing) so the Rust ``ados-protocol`` crate can assert
byte-for-byte parity against the Python implementation it must interoperate
with. Run from the agent repo with its venv:

    .venv/bin/python crates/ados-protocol/tests/interop/generate_fixtures.py

It writes ``fixtures.json`` next to this script. Regenerate and commit whenever
a wire contract changes; CI builds the Rust crate against the committed file.
"""

from __future__ import annotations

import json
from pathlib import Path

import msgpack

from ados.core.contracts import contract_version
from ados.plugins.rpc import CapabilityToken, Envelope, TokenIssuer, encode_frame

# Fixed inputs so the output is deterministic and reproducible.
SECRET = b"interop-secret-0123456789abcdef!"  # 32 bytes
PLUGIN_ID = "com.altnautica.example"
SESSION_ID = "0123456789abcdef"
ISSUED_AT = 1_700_000_000
TTL = 600
EXPIRES_AT = ISSUED_AT + TTL
GRANTED = {"mavlink.read", "event.publish", "telemetry.read"}


def token_fixture() -> dict:
    issuer = TokenIssuer(secret=SECRET)
    sig = issuer._sign(PLUGIN_ID, SESSION_ID, ISSUED_AT, EXPIRES_AT, frozenset(GRANTED))
    token = CapabilityToken(
        plugin_id=PLUGIN_ID,
        session_id=SESSION_ID,
        granted_caps=frozenset(GRANTED),
        issued_at=ISSUED_AT,
        expires_at=EXPIRES_AT,
        signature=sig,
    )
    return {
        "secret_hex": SECRET.hex(),
        "plugin_id": PLUGIN_ID,
        "session_id": SESSION_ID,
        "issued_at": ISSUED_AT,
        "expires_at": EXPIRES_AT,
        "ttl": TTL,
        "granted_caps": sorted(GRANTED),
        "signature": sig,
        "token_string": token.to_string(),
    }


def envelope_fixture() -> dict:
    env = Envelope(
        type="request",
        method="event.publish",
        capability="event.publish",
        args={"topic": "demo", "n": 7},
        request_id="req-001",
        token="v1|p|s|0|600||deadbeef",
        version=1,
        error=None,
    )
    frame = encode_frame(env)
    body = msgpack.packb(env.to_dict(), use_bin_type=True)
    return {
        "type": env.type,
        "method": env.method,
        "capability": env.capability,
        "args": env.args,
        "request_id": env.request_id,
        "token": env.token,
        "version": env.version,
        "error": env.error,
        "frame_hex": frame.hex(),
        "body_hex": body.hex(),
    }


def _sample_state() -> dict:
    return {
        "armed": False,
        "mode": "STABILIZE",
        "battery": {"voltage": 16.4, "remaining": 87},
        "gps": {"fix": 3, "sats": 14},
    }


def state_v1_fixture() -> dict:
    # Mirrors core/ipc.py StateIPCServer.publish (v1): json.dumps(state) + "\n".
    state = _sample_state()
    wire = json.dumps(state).encode() + b"\n"
    return {"state": state, "wire_hex": wire.hex()}


def state_v2_fixture() -> dict:
    # Mirrors core/ipc.py _encode_state_frame (v2) body: the msgpack map
    # {"v": <version>, "s": state} (use_bin_type). The Rust decoder unwraps it
    # back to the inner state. Version sourced from the shared contract registry.
    state = _sample_state()
    version = contract_version("state.v2")
    body = msgpack.packb({"v": version, "s": state}, use_bin_type=True)
    return {"state": state, "version": version, "body_hex": body.hex()}


def main() -> None:
    out = {
        "token": token_fixture(),
        "envelope": envelope_fixture(),
        "state_v1": state_v1_fixture(),
        "state_v2": state_v2_fixture(),
    }
    dest = Path(__file__).with_name("fixtures.json")
    dest.write_text(json.dumps(out, indent=2, sort_keys=True) + "\n")
    print(f"wrote {dest}")


if __name__ == "__main__":
    main()
