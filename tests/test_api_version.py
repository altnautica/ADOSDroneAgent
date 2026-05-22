"""Tests for /api/version capability negotiation endpoint.

Locks the response shape so any future change is forced to either
preserve it or bump api_version. Catches the kind of silent drift
that happens when agent endpoints land without the GCS knowing whether
the agent supports them.
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados import __version__
from ados.api.routes.version import API_VERSION, CAPABILITIES
from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client():
    app = build_api_runtime(uptime_seconds=0.0)
    return TestClient(create_app(app))


def test_version_endpoint_returns_expected_shape(client):
    resp = client.get("/api/version")
    assert resp.status_code == 200
    data = resp.json()
    assert set(data.keys()) == {"api_version", "agent_version", "capabilities"}


def test_api_version_is_string(client):
    resp = client.get("/api/version")
    data = resp.json()
    assert isinstance(data["api_version"], str)
    assert data["api_version"] == API_VERSION


def test_agent_version_matches_package_version(client):
    resp = client.get("/api/version")
    data = resp.json()
    assert data["agent_version"] == __version__


def test_capabilities_is_list_of_strings(client):
    resp = client.get("/api/version")
    data = resp.json()
    assert isinstance(data["capabilities"], list)
    for cap in data["capabilities"]:
        assert isinstance(cap, str)
        assert "." in cap, f"capability flag {cap!r} should be dot-namespaced"


def test_capabilities_includes_known_flags(client):
    resp = client.get("/api/version")
    data = resp.json()
    caps = set(data["capabilities"])
    # Sanity: at least the core endpoints we know shipped before today are present.
    expected = {
        "version.endpoint",
        "status.full",
        "services.control",
        "video.pipeline",
        "wfb.link",
        "scripts.runtime",
        "ota.updater",
        "pairing.mnemonic",
        "ground_station.profile",
        "ros.environment",
        "signing.mavlink",
    }
    missing = expected - caps
    assert not missing, f"missing expected capability flags: {missing}"


def test_capabilities_constant_is_unique():
    """No accidental duplicate flags in the canonical list."""
    assert len(set(CAPABILITIES)) == len(CAPABILITIES)


# ---------------------------------------------------------------------------
# Cross-repo contract — capability list shared with ADOSMissionControl
# ---------------------------------------------------------------------------
#
# The GCS has the mirror test at:
#   ADOSMissionControl/tests/contract/agent-version-contract.test.ts
#
# Both literals below must stay in lockstep. When you add or remove a
# flag from CAPABILITIES in ados/api/routes/version.py, update BOTH:
#   1. AGENT_CAPABILITIES_FROZEN here
#   2. AGENT_CAPABILITIES_FROZEN in the GCS contract test
#
# The two-sided lock catches regressions where an agent endpoint lands
# without the GCS knowing whether the agent supports it. If the lists
# drift, one side's test fails with a clear contract drift message.

AGENT_CAPABILITIES_FROZEN: tuple[str, ...] = (
    "status.full",
    "version.endpoint",
    "services.control",
    "video.pipeline",
    "wfb.link",
    "scripts.runtime",
    "ota.updater",
    "pairing.mnemonic",
    "peripherals.registry",
    "fleet.roster",
    "features.catalog",
    "ground_station.profile",
    "ros.environment",
    "signing.mavlink",
    "webrtc.signaling.last_error",
    "can.passthrough",
)


def test_capabilities_match_frozen_contract_with_gcs():
    """Any change to CAPABILITIES requires updating BOTH this constant
    and the matching constant in the GCS contract test. If only one
    side updates, this test fails with a clear drift message."""
    actual = tuple(CAPABILITIES)
    assert actual == AGENT_CAPABILITIES_FROZEN, (
        "Agent CAPABILITIES drifted from the cross-repo contract.\n"
        "If this is intentional, update BOTH:\n"
        "  - AGENT_CAPABILITIES_FROZEN in this file\n"
        "  - AGENT_CAPABILITIES_FROZEN in "
        "ADOSMissionControl/tests/contract/agent-version-contract.test.ts\n"
        f"Expected (frozen): {AGENT_CAPABILITIES_FROZEN}\n"
        f"Actual (current):  {actual}"
    )
