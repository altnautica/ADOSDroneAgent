"""Tests for /api/version capability negotiation endpoint.

Locks the response shape so any future change is forced to either
preserve it or bump api_version. Catches the kind of silent drift
that happens when agent endpoints land without the GCS knowing whether
the agent supports them.
"""

from __future__ import annotations

from ados.api.routes.version import CAPABILITIES


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
    "pairing.bind_state",
    "peripherals.registry",
    "fleet.roster",
    "features.catalog",
    "ground_station.profile",
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
