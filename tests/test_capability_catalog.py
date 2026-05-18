"""Tests for the plugin capability catalog and its REST surface.

The catalog is the single source of truth for human-readable
capability metadata. These tests pin three invariants:

1. Every capability id in :data:`AGENT_CAPABILITIES` has a catalog
   entry, and every catalog entry covers an id in the canonical set.
2. The REST parse + install endpoints reject manifests that declare
   an unknown capability id so the operator never sees a permission
   row with no label.
3. The successful response shape carries the inlined label,
   description, category, risk, and risk_reason fields.
"""

from __future__ import annotations

import zipfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from ados.api.routes.plugins import _set_supervisor_for_tests
from ados.api.server import create_app
from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.capabilities import (
    AGENT_CAPABILITIES,
    CAPABILITY_CATALOG,
    CapabilityMeta,
    get_capability_meta,
    is_known_capability,
)
from ados.plugins.supervisor import PluginSupervisor
from tests.api_runtime_utils import build_api_runtime


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
        "vehicle.command",
        "mission.write",
    }
    for cap_id in expected_high_or_critical:
        meta = CAPABILITY_CATALOG[cap_id]
        assert meta["risk"] in {"high", "critical"}, (
            f"{cap_id} should be high/critical, got {meta['risk']!r}"
        )


# ---------------------------------------------------------------------
# Helper lookups
# ---------------------------------------------------------------------


def test_get_capability_meta_returns_entry_for_known_id():
    meta = get_capability_meta("mavlink.read")
    assert meta is not None
    assert meta["category"] == "flight_control"


def test_get_capability_meta_returns_none_for_unknown_id():
    assert get_capability_meta("not.a.real.capability") is None


def test_is_known_capability():
    assert is_known_capability("event.publish") is True
    assert is_known_capability("not.a.real.capability") is False


# ---------------------------------------------------------------------
# REST surface — parse + install enrichment and rejection
# ---------------------------------------------------------------------


@pytest.fixture
def agent_app():
    return build_api_runtime(uptime_seconds=0.0)


@pytest.fixture
def client(agent_app):
    return TestClient(create_app(agent_app))


@pytest.fixture
def isolated_paths(tmp_path: Path, monkeypatch):
    install_dir = tmp_path / "var-plugins"
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    keys_dir = tmp_path / "keys"
    keys_dir.mkdir()
    log_dir = tmp_path / "log"
    log_dir.mkdir()
    unit_dir = tmp_path / "systemd"
    unit_dir.mkdir()
    state_path = state_dir / "plugin-state.json"
    monkeypatch.setattr(
        "ados.plugins.state.PLUGIN_STATE_PATH", state_path, raising=False
    )
    monkeypatch.setattr(
        "ados.plugins.signing.PLUGIN_KEYS_DIR", keys_dir, raising=False
    )
    monkeypatch.setattr(
        "ados.plugins.systemd.PLUGIN_UNIT_DIR", unit_dir, raising=False
    )
    monkeypatch.setattr(
        "ados.plugins.systemd.PLUGIN_LOG_DIR", log_dir, raising=False
    )
    from ados.plugins import systemd as systemd_mod

    monkeypatch.setattr(
        systemd_mod,
        "PLUGIN_SLICE_PATH",
        unit_dir / systemd_mod.PLUGIN_SLICE_NAME,
        raising=False,
    )
    return {"install_dir": install_dir, "unit_dir": unit_dir}


@pytest.fixture
def supervisor(isolated_paths):
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    _set_supervisor_for_tests(sup)
    yield sup
    _set_supervisor_for_tests(None)


def _build_archive(
    tmp_path: Path,
    plugin_id: str,
    permissions: list[str],
) -> Path:
    perms_yaml = ", ".join(f'"{p}"' for p in permissions)
    manifest_yaml = f"""\
schema_version: 1
id: {plugin_id}
version: 0.1.0
name: Catalog Test
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: [{perms_yaml}]
  resources:
    max_ram_mb: 32
    max_cpu_percent: 10
    max_pids: 4
"""
    archive_path = tmp_path / f"{plugin_id}.adosplug"
    with zipfile.ZipFile(archive_path, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, manifest_yaml)
        zf.writestr("agent/plugin.py", "# stub\n")
    return archive_path


def test_parse_enriches_each_permission_with_catalog_metadata(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(
        tmp_path,
        "com.example.parse-catalog",
        ["event.publish", "mavlink.read"],
    )
    raw = archive_path.read_bytes()
    resp = client.post(
        "/api/plugins/parse",
        files={
            "file": (
                "com.example.parse-catalog.adosplug",
                raw,
                "application/zip",
            )
        },
    )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["ok"] is True
    perms = {p["id"]: p for p in body["permissions"]}
    assert set(perms) == {"event.publish", "mavlink.read"}
    for entry in perms.values():
        assert entry["label"], entry
        assert entry["description"], entry
        assert entry["category"] in {
            "hardware",
            "flight_control",
            "data_network",
            "compute_process",
            "ui_slot",
        }
        assert entry["risk"] in {"low", "medium", "high", "critical"}
        assert entry["risk_reason"], entry


def test_parse_rejects_manifest_with_unknown_capability(
    client, supervisor, tmp_path: Path
):
    # The agent-side schema validator only warns on unknown caps so
    # the manifest still loads; the REST endpoint is responsible for
    # the hard rejection because the install dialog cannot render an
    # unlabelled permission row.
    archive_path = _build_archive(
        tmp_path,
        "com.example.unknown-cap",
        ["event.publish", "not.a.real.capability"],
    )
    raw = archive_path.read_bytes()
    resp = client.post(
        "/api/plugins/parse",
        files={
            "file": (
                "com.example.unknown-cap.adosplug",
                raw,
                "application/zip",
            )
        },
    )
    assert resp.status_code == 400, resp.text
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "manifest_invalid"
    assert "Unknown capability" in body["detail"]
    assert "not.a.real.capability" in body["detail"]


def test_install_rejects_manifest_with_unknown_capability(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(
        tmp_path,
        "com.example.install-unknown",
        ["event.publish", "not.a.real.capability"],
    )
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        resp = client.post(
            "/api/plugins/install",
            files={
                "file": (
                    "com.example.install-unknown.adosplug",
                    raw,
                    "application/zip",
                )
            },
        )
    assert resp.status_code == 400, resp.text
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "manifest_invalid"
    assert "Unknown capability" in body["detail"]
    # Pre-flight rejection means no install record on disk.
    listing = client.get("/api/plugins").json()
    assert listing["installs"] == []


def test_detail_endpoint_enriches_permissions(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(
        tmp_path,
        "com.example.detail-catalog",
        ["event.publish"],
    )
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        client.post(
            "/api/plugins/install",
            files={
                "file": (
                    "com.example.detail-catalog.adosplug",
                    raw,
                    "application/zip",
                )
            },
        )
    detail = client.get("/api/plugins/com.example.detail-catalog").json()
    perms = detail["manifest"]["permissions"]
    assert len(perms) == 1
    entry = perms[0]
    assert entry["id"] == "event.publish"
    assert entry["label"]
    assert entry["category"] == "data_network"
    assert entry["risk"] in {"low", "medium", "high", "critical"}


def test_capability_meta_typed_dict_shape():
    # Round-trip a literal through the TypedDict to keep mypy
    # honest if anyone introduces a typo on the type later.
    sample: CapabilityMeta = {
        "label": "x",
        "description": "y",
        "category": "data_network",
        "risk": "low",
        "risk_reason": "z",
    }
    assert sample["label"] == "x"
