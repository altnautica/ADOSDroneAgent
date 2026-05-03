"""Tests for the plugin lifecycle REST endpoints.

The endpoints sit on top of :class:`PluginSupervisor`, so the
fixtures here build a real supervisor against ``tmp_path`` (no
``/var/ados``, no real systemctl) and inject it via the test seam in
the route module.
"""

from __future__ import annotations

import time
import zipfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from ados.api.routes.plugins import _set_supervisor_for_tests
from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.service_tracker import ServiceTracker
from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.supervisor import PluginSupervisor
from ados.services.mavlink.state import VehicleState


@pytest.fixture
def agent_app():
    app = MagicMock()
    app.config = ADOSConfig()
    app.health = HealthMonitor()
    app.services = ServiceTracker()
    app._start_time = time.monotonic()
    app.uptime_seconds = 0.0
    app._vehicle_state = VehicleState()
    app._fc_connection = MagicMock()
    app._fc_connection.connected = False
    app._tasks = []
    app._param_cache = None
    app.pairing_manager.is_paired = False
    return app


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
    return {
        "install_dir": install_dir,
        "unit_dir": unit_dir,
    }


@pytest.fixture
def supervisor(isolated_paths):
    sup = PluginSupervisor(
        install_dir=isolated_paths["install_dir"], require_signed=False
    )
    sup.discover()
    _set_supervisor_for_tests(sup)
    yield sup
    _set_supervisor_for_tests(None)


def _build_archive(tmp_path: Path, plugin_id: str = "com.example.basic") -> Path:
    manifest_yaml = f"""\
schema_version: 1
id: {plugin_id}
version: 0.1.0
name: Basic
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["event.publish"]
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


# ---------------------------------------------------------------------
# List + detail
# ---------------------------------------------------------------------


def test_list_plugins_empty(client, supervisor):
    resp = client.get("/api/plugins")
    assert resp.status_code == 200
    assert resp.json() == {"installs": []}


def test_get_plugin_404_when_missing(client, supervisor):
    resp = client.get("/api/plugins/com.example.absent")
    assert resp.status_code == 404
    body = resp.json()
    assert body["ok"] is False
    assert body["code"] == 14
    assert body["kind"] == "not_found"


# ---------------------------------------------------------------------
# Install + lifecycle
# ---------------------------------------------------------------------


def test_install_round_trip(client, supervisor, tmp_path: Path):
    archive_path = _build_archive(tmp_path)
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        resp = client.post(
            "/api/plugins/install",
            files={"file": ("com.example.basic.adosplug", raw, "application/zip")},
        )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["ok"] is True
    assert body["plugin_id"] == "com.example.basic"
    assert body["risk"] == "low"
    assert "event.publish" in body["permissions_requested"]

    # Now visible on the list endpoint.
    listing = client.get("/api/plugins").json()
    assert len(listing["installs"]) == 1
    assert listing["installs"][0]["plugin_id"] == "com.example.basic"

    # Detail endpoint returns the manifest.
    detail = client.get("/api/plugins/com.example.basic").json()
    assert detail["manifest"]["risk"] == "low"
    assert "agent" in detail["manifest"]["halves"]


def test_install_rejects_non_adosplug(client, supervisor, tmp_path: Path):
    bad = tmp_path / "not-a-plug.txt"
    bad.write_text("hello", encoding="utf-8")
    raw = bad.read_bytes()
    resp = client.post(
        "/api/plugins/install",
        files={"file": ("not-a-plug.txt", raw, "text/plain")},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["code"] == 2
    assert body["kind"] == "usage_error"


def test_grant_unknown_permission_returns_permission_deny(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(tmp_path)
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        client.post(
            "/api/plugins/install",
            files={"file": ("com.example.basic.adosplug", raw, "application/zip")},
        )
    resp = client.post(
        "/api/plugins/com.example.basic/grant",
        json={"permission_id": "vehicle.command"},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["code"] == 11
    assert body["kind"] == "permission_deny"


def test_remove_unknown_returns_404(client, supervisor):
    resp = client.delete("/api/plugins/com.example.absent")
    assert resp.status_code == 404
    body = resp.json()
    assert body["code"] == 14


def test_parse_returns_manifest_summary_without_committing(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(tmp_path)
    raw = archive_path.read_bytes()
    resp = client.post(
        "/api/plugins/parse",
        files={"file": ("p.adosplug", raw, "application/zip")},
    )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["ok"] is True
    assert body["plugin_id"] == "com.example.basic"
    assert body["risk"] == "low"
    assert body["signed"] is False
    assert body["signer_id"] is None
    assert "agent" in body["halves"]
    assert any(p["id"] == "event.publish" for p in body["permissions"])
    # Critically, parse must NOT have created an install row.
    assert client.get("/api/plugins").json() == {"installs": []}


def test_parse_rejects_non_adosplug(client, supervisor, tmp_path: Path):
    bad = tmp_path / "x.txt"
    bad.write_text("oops", encoding="utf-8")
    resp = client.post(
        "/api/plugins/parse",
        files={"file": ("x.txt", bad.read_bytes(), "text/plain")},
    )
    assert resp.status_code == 400
    assert resp.json()["code"] == 2


def test_parse_rejects_malformed_archive(client, supervisor):
    resp = client.post(
        "/api/plugins/parse",
        files={"file": ("p.adosplug", b"not a zip", "application/zip")},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["code"] == 12


def test_full_lifecycle_install_grant_enable_disable_remove(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(tmp_path)
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        # Install
        r = client.post(
            "/api/plugins/install",
            files={"file": ("com.example.basic.adosplug", raw, "application/zip")},
        )
        assert r.status_code == 200, r.text
        # Grant
        r = client.post(
            "/api/plugins/com.example.basic/grant",
            json={"permission_id": "event.publish"},
        )
        assert r.status_code == 200
        # Enable
        r = client.post("/api/plugins/com.example.basic/enable")
        assert r.status_code == 200
        # Disable
        r = client.post("/api/plugins/com.example.basic/disable")
        assert r.status_code == 200
        # Remove
        r = client.delete("/api/plugins/com.example.basic")
        assert r.status_code == 200

    # Listing is empty again.
    assert client.get("/api/plugins").json() == {"installs": []}
