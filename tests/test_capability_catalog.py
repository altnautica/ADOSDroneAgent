"""Tests for the plugin capability catalog.

The catalog is the single source of truth for human-readable
capability metadata. These tests pin its invariants: every capability
id in :data:`AGENT_CAPABILITIES` has a catalog entry, every catalog
entry covers an id in the canonical set, and each entry carries the
label, description, category, risk, and risk_reason the install dialog
and ``ados plugin lint`` render.
"""

from __future__ import annotations

from ados.plugins.capabilities import AGENT_CAPABILITIES, CAPABILITY_CATALOG

# ---------------------------------------------------------------------
# Catalog completeness
# ---------------------------------------------------------------------


def test_every_agent_capability_has_catalog_entry():
    missing = AGENT_CAPABILITIES - CAPABILITY_CATALOG.keys()
    assert not missing, (
        f"AGENT_CAPABILITIES missing catalog entries: {sorted(missing)}"
    )


def test_no_catalog_entries_outside_agent_capabilities():
    orphan = CAPABILITY_CATALOG.keys() - AGENT_CAPABILITIES
    assert not orphan, (
        f"CAPABILITY_CATALOG has entries not in AGENT_CAPABILITIES: "
        f"{sorted(orphan)}"
    )


def test_catalog_entries_have_required_fields():
    required_keys = {"label", "description", "category", "risk", "risk_reason"}
    allowed_categories = {
        "hardware",
        "flight_control",
        "data_network",
        "compute_process",
        "ui_slot",
    }
    allowed_risk = {"low", "medium", "high", "critical"}
    for cap_id, meta in CAPABILITY_CATALOG.items():
        keys = set(meta.keys())
        assert required_keys <= keys, (
            f"{cap_id} missing fields: {required_keys - keys}"
        )
        assert meta["category"] in allowed_categories, (
            f"{cap_id} has invalid category {meta['category']!r}"
        )
        assert meta["risk"] in allowed_risk, (
            f"{cap_id} has invalid risk {meta['risk']!r}"
        )
        # Labels should be sentence-case action verbs, short.
        assert len(meta["label"]) <= 120, f"{cap_id} label too long"
        assert len(meta["description"]) >= 20, (
            f"{cap_id} description too terse"
        )


def test_high_risk_caps_match_spec():
    expected_high_or_critical = {
        "mavlink.write",
        "mavlink.component.vio",
        "estimator.pose.inject",
        "process.spawn",
        "mission.write",
    }
    for cap_id in expected_high_or_critical:
        meta = CAPABILITY_CATALOG[cap_id]
        assert meta["risk"] in {"high", "critical"}, (
            f"{cap_id} should be high/critical, got {meta['risk']!r}"
        )
