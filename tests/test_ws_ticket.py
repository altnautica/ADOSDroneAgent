"""Self-contained WS ticket verify/mint + cross-language parity with Rust."""

from __future__ import annotations

import json

from ados.core.ws_ticket import (
    SCOPE_MAVLINK_WS,
    load_pairing_api_key,
    mint_ticket,
    verify_ticket,
)


def test_cross_language_vector_matches_rust():
    """The exact token the Rust `ados_protocol::ws_ticket` known-answer test
    pins. If this drifts, cross-language tickets silently stop verifying — the
    two implementations MUST change in lockstep."""
    token = mint_ticket(
        SCOPE_MAVLINK_WS, api_key="ados_secret", ttl_seconds=30, now=1_000_000
    )
    assert token == (
        "v1|gs.mavlink_ws|1000000|1000030|"
        "655a695c0b38fa07b830a7ca3534a4cd6ef95831fb5e523cc98871bbef191413"
    )


def test_round_trip_verifies():
    token = mint_ticket(SCOPE_MAVLINK_WS, api_key="k", ttl_seconds=30, now=1000)
    assert verify_ticket(token, expected_scope=SCOPE_MAVLINK_WS, api_key="k", now=1000)
    assert verify_ticket(token, expected_scope=SCOPE_MAVLINK_WS, api_key="k", now=1029)


def test_rejects_expiry_scope_key_and_tamper():
    token = mint_ticket(SCOPE_MAVLINK_WS, api_key="k", ttl_seconds=30, now=1000)
    # Expired at the boundary (now >= expires).
    assert not verify_ticket(
        token, expected_scope=SCOPE_MAVLINK_WS, api_key="k", now=1030
    )
    # Wrong scope.
    assert not verify_ticket(
        token, expected_scope="gs.pic_events", api_key="k", now=1000
    )
    # Wrong key.
    assert not verify_ticket(
        token, expected_scope=SCOPE_MAVLINK_WS, api_key="other", now=1000
    )
    # Tampered expiry, original signature.
    parts = token.split("|")
    forged = f"v1|{parts[1]}|{parts[2]}|99999|{parts[4]}"
    assert not verify_ticket(
        forged, expected_scope=SCOPE_MAVLINK_WS, api_key="k", now=1000
    )


def test_rejects_malformed():
    for bad in ["", "v2|s|1|2|ff", "v1|s|notanint|2|ff", "v1|s|1|2|nothex"]:
        assert not verify_ticket(bad, expected_scope="s", api_key="k", now=0)


def test_load_pairing_api_key(tmp_path):
    p = tmp_path / "pairing.json"
    p.write_text(json.dumps({"paired": True, "api_key": "secret"}))
    assert load_pairing_api_key(p) == "secret"
    # Not paired, empty key, and absent all read as None.
    p.write_text(json.dumps({"paired": False, "api_key": "secret"}))
    assert load_pairing_api_key(p) is None
    p.write_text(json.dumps({"paired": True, "api_key": ""}))
    assert load_pairing_api_key(p) is None
    assert load_pairing_api_key(tmp_path / "absent.json") is None
