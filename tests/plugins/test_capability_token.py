"""Tests for the dual-issuer capability-token mint + verify path.

Three issuer variants:

* ``iss = "cloud:<userId>"`` — operator HMAC secret fetched from Convex
* ``iss = "agent:<deviceId>"`` — HKDF-derived per-pairing secret
* ``iss = "local"`` — dev-mode CLI bypass
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import time

import pytest

from ados.api.routes._plugins_helpers import (
    derive_agent_token_secret,
    mint_agent_capability_token,
    parse_token_string,
    verify_agent_token_signature,
)
from ados.plugins.rpc import (
    MultiIssuerVerifier,
    TokenInvalid,
)

# ---------------------------------------------------------------------
# HKDF derivation + agent issuer mint/verify
# ---------------------------------------------------------------------


def test_hkdf_secret_is_deterministic_and_32_bytes():
    a = derive_agent_token_secret("pairing-key-abc")
    b = derive_agent_token_secret("pairing-key-abc")
    assert a == b
    assert len(a) == 32

    different = derive_agent_token_secret("other-pairing-key")
    assert different != a


def test_hkdf_secret_empty_pairing_key_raises():
    with pytest.raises(ValueError):
        derive_agent_token_secret("")


def test_mint_agent_token_round_trip():
    token, claims = mint_agent_capability_token(
        plugin_id="com.example.plug",
        agent_id="device-001",
        operator_id="op-99",
        granted_capabilities=["event.publish", "telemetry.read"],
        pairing_key="paired-key",
        ttl_seconds=600,
    )
    # Token shape: <claims_b64>.<sig_b64>
    assert "." in token

    verified = verify_agent_token_signature(token=token, pairing_key="paired-key")
    assert verified["pluginId"] == "com.example.plug"
    assert verified["agentId"] == "device-001"
    assert verified["iss"] == "agent:device-001"
    # Granted caps are sorted + de-duplicated.
    assert verified["grantedCapabilities"] == ["event.publish", "telemetry.read"]
    assert verified["expiresAt"] > int(time.time() * 1000)


def test_mint_agent_token_wrong_pairing_key_fails_verify():
    token, _ = mint_agent_capability_token(
        plugin_id="com.example.plug",
        agent_id="device-001",
        operator_id="op-99",
        granted_capabilities=["event.publish"],
        pairing_key="paired-key",
    )
    with pytest.raises(ValueError, match="signature mismatch"):
        verify_agent_token_signature(token=token, pairing_key="not-the-same-key")


def test_mint_agent_token_expired_rejected():
    token, _ = mint_agent_capability_token(
        plugin_id="com.example.plug",
        agent_id="device-001",
        operator_id="op-99",
        granted_capabilities=[],
        pairing_key="paired-key",
        ttl_seconds=-1,  # already expired
    )
    with pytest.raises(ValueError, match="expired"):
        verify_agent_token_signature(token=token, pairing_key="paired-key")


# ---------------------------------------------------------------------
# Multi-issuer verifier — 3 paths
# ---------------------------------------------------------------------


def _build_token(claims: dict, secret: bytes) -> str:
    blob = json.dumps(claims, sort_keys=True, separators=(",", ":")).encode("utf-8")
    sig = hmac.new(secret, blob, hashlib.sha256).digest()
    return (
        base64.urlsafe_b64encode(blob).decode("ascii").rstrip("=")
        + "."
        + base64.urlsafe_b64encode(sig).decode("ascii").rstrip("=")
    )


def test_verifier_cloud_issuer_happy_path():
    operator_secret = b"\x11" * 32
    claims = {
        "pluginId": "com.example.plug",
        "agentId": "device-001",
        "operatorId": "op-99",
        "expiresAt": int(time.time() * 1000) + 60_000,
        "grantedCapabilities": ["event.publish"],
        "iss": "cloud:op-99",
    }
    token = _build_token(claims, operator_secret)
    verifier = MultiIssuerVerifier(
        device_id="device-001",
        operator_secret_fetcher=lambda uid: operator_secret if uid == "op-99" else None,
    )
    verified = verifier.verify(token)
    assert verified.issuer == "cloud:op-99"
    assert verified.plugin_id == "com.example.plug"
    assert verified.agent_id == "device-001"


def test_verifier_cloud_issuer_rejects_wrong_device():
    operator_secret = b"\x22" * 32
    claims = {
        "pluginId": "p",
        "agentId": "device-OTHER",
        "operatorId": "op-99",
        "expiresAt": int(time.time() * 1000) + 60_000,
        "grantedCapabilities": [],
        "iss": "cloud:op-99",
    }
    token = _build_token(claims, operator_secret)
    verifier = MultiIssuerVerifier(
        device_id="device-001",
        operator_secret_fetcher=lambda uid: operator_secret,
    )
    with pytest.raises(TokenInvalid, match="agentId"):
        verifier.verify(token)


def test_verifier_cloud_issuer_caches_operator_secret(monkeypatch):
    operator_secret = b"\x33" * 32
    calls = {"n": 0}

    def fetcher(uid: str) -> bytes:
        calls["n"] += 1
        return operator_secret

    claims = {
        "pluginId": "p",
        "agentId": "device-1",
        "operatorId": "u-1",
        "expiresAt": int(time.time() * 1000) + 60_000,
        "grantedCapabilities": [],
        "iss": "cloud:u-1",
    }
    token = _build_token(claims, operator_secret)
    verifier = MultiIssuerVerifier(
        device_id="device-1", operator_secret_fetcher=fetcher
    )
    verifier.verify(token)
    verifier.verify(token)
    assert calls["n"] == 1


def test_verifier_agent_issuer_uses_provider():
    agent_secret = derive_agent_token_secret("pairing-key-xyz")
    token, _ = mint_agent_capability_token(
        plugin_id="com.example.plug",
        agent_id="device-001",
        operator_id="op-1",
        granted_capabilities=["telemetry.read"],
        pairing_key="pairing-key-xyz",
    )
    verifier = MultiIssuerVerifier(
        device_id="device-001",
        agent_secret_provider=lambda: agent_secret,
    )
    verified = verifier.verify(token)
    assert verified.issuer == "agent:device-001"
    assert verified.plugin_id == "com.example.plug"


def test_verifier_agent_issuer_rejects_wrong_secret():
    token, _ = mint_agent_capability_token(
        plugin_id="com.example.plug",
        agent_id="device-001",
        operator_id="op-1",
        granted_capabilities=[],
        pairing_key="real-key",
    )
    verifier = MultiIssuerVerifier(
        device_id="device-001",
        agent_secret_provider=lambda: derive_agent_token_secret("other-key"),
    )
    with pytest.raises(TokenInvalid, match="signature mismatch"):
        verifier.verify(token)


def test_verifier_local_issuer_skips_agent_id_check():
    dev_secret = b"\x77" * 32
    claims = {
        "pluginId": "p",
        "agentId": "WHATEVER",  # intentionally not matching device_id
        "operatorId": "dev",
        "expiresAt": int(time.time() * 1000) + 60_000,
        "grantedCapabilities": ["any"],
        "iss": "local",
    }
    token = _build_token(claims, dev_secret)
    verifier = MultiIssuerVerifier(
        device_id="device-real",
        local_dev_secret=dev_secret,
    )
    verified = verifier.verify(token)
    assert verified.issuer == "local"


def test_verifier_local_issuer_disabled_when_no_secret():
    claims = {
        "pluginId": "p",
        "agentId": "a",
        "operatorId": "dev",
        "expiresAt": int(time.time() * 1000) + 60_000,
        "grantedCapabilities": [],
        "iss": "local",
    }
    token = _build_token(claims, b"\xff" * 32)
    verifier = MultiIssuerVerifier(device_id="device-real")  # no dev secret
    with pytest.raises(TokenInvalid, match="local issuer not enabled"):
        verifier.verify(token)


def test_verifier_rejects_expired_token():
    operator_secret = b"\x44" * 32
    claims = {
        "pluginId": "p",
        "agentId": "device-1",
        "operatorId": "u",
        "expiresAt": int(time.time() * 1000) - 1000,
        "grantedCapabilities": [],
        "iss": "cloud:u",
    }
    token = _build_token(claims, operator_secret)
    verifier = MultiIssuerVerifier(
        device_id="device-1",
        operator_secret_fetcher=lambda uid: operator_secret,
    )
    with pytest.raises(TokenInvalid, match="expired"):
        verifier.verify(token)


def test_verifier_unknown_issuer_rejected():
    claims = {
        "pluginId": "p",
        "agentId": "device-1",
        "operatorId": "u",
        "expiresAt": int(time.time() * 1000) + 60_000,
        "grantedCapabilities": [],
        "iss": "mystery",
    }
    token = _build_token(claims, b"\x55" * 32)
    verifier = MultiIssuerVerifier(device_id="device-1")
    with pytest.raises(TokenInvalid, match="unknown issuer"):
        verifier.verify(token)


def test_parse_token_string_rejects_malformed():
    with pytest.raises(ValueError):
        parse_token_string("not-a-token")
