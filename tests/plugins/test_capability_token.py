"""Tests for the agent-issued capability-token mint + verify helpers."""

from __future__ import annotations

import time

import pytest

from ados.api.routes._plugins_helpers import (
    compute_granted_caps_for_token,
    derive_agent_token_secret,
    mint_agent_capability_token,
    parse_token_string,
    verify_agent_token_signature,
)
from ados.plugins.state import PermissionGrant

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


def test_token_grants_follow_the_install_record():
    """A revoke must leave the token and a later LAN grant must reach it."""
    permissions = {
        "event.publish": PermissionGrant(granted=True, granted_at=1),
        "vehicle.command": PermissionGrant(granted=False, granted_at=1, revoked_at=2),
        "telemetry.read": PermissionGrant(granted=True, granted_at=3),
    }
    assert compute_granted_caps_for_token(permissions) == [
        "event.publish",
        "telemetry.read",
    ]


def test_parse_token_string_rejects_malformed():
    with pytest.raises(ValueError):
        parse_token_string("not-a-token")
