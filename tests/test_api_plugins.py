"""Tests for the plugin lifecycle REST endpoints.

The endpoints sit on top of :class:`PluginSupervisor`, so the
fixtures here build a real supervisor against ``tmp_path`` (no
``/var/ados``, no real systemctl) and inject it via the test seam in
the route module.
"""

from __future__ import annotations

import hashlib
import zipfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from ados.api.routes.plugins import _set_supervisor_for_tests
from ados.api.server import create_app
from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.supervisor import PluginSupervisor
from tests.api_runtime_utils import build_api_runtime


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
    # A plugin with no tools surfaces an empty mcp block.
    assert detail["manifest"]["mcp"] == {"tools": [], "resources": [], "prompts": []}


def test_get_plugin_surfaces_mcp_tools(client, supervisor, tmp_path: Path):
    manifest_yaml = """\
schema_version: 3
id: com.example.tools
version: 0.1.0
name: Tools
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions: ["event.publish", "mcp.expose"]
  contributes:
    tools:
      - name: start_follow
        title: Start following
        safety_class: flight_action
        inputSchema: {type: object}
      - name: stop_follow
        safety_class: safe_write
"""
    archive_path = tmp_path / "com.example.tools.adosplug"
    with zipfile.ZipFile(archive_path, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, manifest_yaml)
        zf.writestr("agent/plugin.py", "# stub\n")
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        resp = client.post(
            "/api/plugins/install",
            files={"file": ("com.example.tools.adosplug", raw, "application/zip")},
        )
    assert resp.status_code == 200, resp.text

    detail = client.get("/api/plugins/com.example.tools").json()
    mcp = detail["manifest"]["mcp"]
    names = [t["name"] for t in mcp["tools"]]
    assert names == ["start_follow", "stop_follow"]
    # Agent-half tools carry the half marker and their declared safety class.
    assert all(t["half"] == "agent" for t in mcp["tools"])
    assert mcp["tools"][0]["safety_class"] == "flight_action"


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


def test_parse_from_url_rejects_a_non_allowlisted_url(client, supervisor):
    resp = client.post(
        "/api/plugins/parse_from_url",
        json={"url": "https://evil.example.com/x.adosplug"},
    )
    assert resp.status_code == 400
    assert resp.json()["code"] == 2  # url_invalid


def test_parse_from_url_returns_the_summary_without_committing(
    client, supervisor, tmp_path: Path, monkeypatch
):
    raw = _build_archive(tmp_path).read_bytes()

    async def fake_stream(*, client, url, dest, expected_sha256=""):
        from ados.plugins.install_from_url_impl import DownloadOutcome

        Path(dest).write_bytes(raw)
        return DownloadOutcome(
            path=Path(dest),
            sha256_hex=hashlib.sha256(raw).hexdigest(),
            byte_count=len(raw),
        )

    monkeypatch.setattr(
        "ados.api.routes.plugins.stream_archive_to_path", fake_stream
    )
    resp = client.post(
        "/api/plugins/parse_from_url",
        json={"url": "https://github.com/altnautica/x/releases/download/v1/p.adosplug"},
    )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["ok"] is True
    assert body["plugin_id"] == "com.example.basic"
    # The archive digest is returned so the GCS can pin the subsequent install.
    assert body["archive_sha256"] == hashlib.sha256(raw).hexdigest()
    # Parse must NOT have created an install row.
    assert client.get("/api/plugins").json() == {"installs": []}


def test_revoke_unknown_plugin_returns_404(client, supervisor):
    resp = client.delete(
        "/api/plugins/com.example.absent/perms/event.publish"
    )
    assert resp.status_code == 404
    body = resp.json()
    assert body["ok"] is False
    assert body["code"] == 14
    assert body["kind"] == "not_found"


def test_revoke_ungranted_permission_is_ok(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(tmp_path)
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        client.post(
            "/api/plugins/install",
            files={
                "file": (
                    "com.example.basic.adosplug",
                    raw,
                    "application/zip",
                )
            },
        )
    # Permission was never granted; revoking succeeds with empty grant set.
    resp = client.delete(
        "/api/plugins/com.example.basic/perms/event.publish"
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    assert body["plugin_id"] == "com.example.basic"
    assert body["granted"] == []
    assert body["requires_restart"] is False


def test_grant_then_revoke_shrinks_granted_set(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(tmp_path)
    raw = archive_path.read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        client.post(
            "/api/plugins/install",
            files={
                "file": (
                    "com.example.basic.adosplug",
                    raw,
                    "application/zip",
                )
            },
        )
    grant_resp = client.post(
        "/api/plugins/com.example.basic/grant",
        json={"permission_id": "event.publish"},
    )
    assert grant_resp.status_code == 200
    detail = client.get("/api/plugins/com.example.basic").json()
    assert detail["install"]["permissions"]["event.publish"]["granted"] is True
    revoke_resp = client.delete(
        "/api/plugins/com.example.basic/perms/event.publish"
    )
    assert revoke_resp.status_code == 200
    body = revoke_resp.json()
    assert body["ok"] is True
    assert body["plugin_id"] == "com.example.basic"
    assert body["granted"] == []
    detail = client.get("/api/plugins/com.example.basic").json()
    assert (
        detail["install"]["permissions"]["event.publish"]["granted"] is False
    )


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


# ---------------------------------------------------------------------
# GCS half: extended /plugins/{id} detail + the LAN bundle-serve route
# ---------------------------------------------------------------------


BUNDLE_JS = 'console.log("gcs boot");export const x=1;'


def _build_gcs_archive(
    tmp_path: Path, plugin_id: str = "com.example.gcs"
) -> Path:
    manifest_yaml = f"""\
schema_version: 1
id: {plugin_id}
version: 0.1.0
name: Gcs Hybrid
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
gcs:
  entrypoint: gcs/plugin.bundle.js
  isolation: iframe
  contributes:
    panels:
      - id: gcs-tab
        slot: drone.detail.tab
        title: Demo
    overlays:
      - id: gcs-overlay
  locales: ["en"]
"""
    archive_path = tmp_path / f"{plugin_id}.adosplug"
    with zipfile.ZipFile(archive_path, "w") as zf:
        zf.writestr(MANIFEST_FILENAME, manifest_yaml)
        zf.writestr("agent/plugin.py", "# stub\n")
        zf.writestr("gcs/plugin.bundle.js", BUNDLE_JS)
        zf.writestr("gcs/extra/asset.js", "// nested\n")
    return archive_path


def _install_gcs(client, tmp_path: Path, plugin_id: str = "com.example.gcs") -> str:
    raw = _build_gcs_archive(tmp_path, plugin_id).read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        resp = client.post(
            "/api/plugins/install",
            files={"file": (f"{plugin_id}.adosplug", raw, "application/zip")},
        )
    assert resp.status_code == 200, resp.text
    return plugin_id


def test_get_plugin_includes_gcs_block_and_granted_caps(
    client, supervisor, tmp_path: Path
):
    pid = _install_gcs(client, tmp_path)
    detail = client.get(f"/api/plugins/{pid}").json()
    gcs = detail["manifest"]["gcs"]
    assert gcs is not None
    assert gcs["entrypoint"] == "gcs/plugin.bundle.js"
    assert gcs["contributes"]["panels"][0]["id"] == "gcs-tab"
    assert gcs["contributes"]["overlays"][0]["id"] == "gcs-overlay"
    assert gcs["locales"] == ["en"]
    assert "gcs" in detail["manifest"]["halves"]
    assert isinstance(detail["granted_capabilities"], list)


def test_get_plugin_gcs_block_none_for_agent_only(
    client, supervisor, tmp_path: Path
):
    raw = _build_archive(tmp_path).read_bytes()
    with patch("ados.plugins.supervisor.subprocess.run") as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        client.post(
            "/api/plugins/install",
            files={
                "file": ("com.example.basic.adosplug", raw, "application/zip")
            },
        )
    detail = client.get("/api/plugins/com.example.basic").json()
    assert detail["manifest"]["gcs"] is None


def test_gcs_asset_route_serves_bundle_and_nested(
    client, supervisor, tmp_path: Path
):
    pid = _install_gcs(client, tmp_path)
    r = client.get(f"/api/plugins/{pid}/gcs/plugin.bundle.js")
    assert r.status_code == 200, r.text
    assert r.text == BUNDLE_JS
    r2 = client.get(f"/api/plugins/{pid}/gcs/extra/asset.js")
    assert r2.status_code == 200
    assert "nested" in r2.text


def test_gcs_asset_route_404s(client, supervisor, tmp_path: Path):
    pid = _install_gcs(client, tmp_path)
    assert client.get(f"/api/plugins/{pid}/gcs/nope.js").status_code == 404
    assert (
        client.get(
            "/api/plugins/com.example.absent/gcs/plugin.bundle.js"
        ).status_code
        == 404
    )


def test_gcs_asset_route_rejects_traversal(client, supervisor, tmp_path: Path):
    pid = _install_gcs(client, tmp_path)
    # An encoded ../ must never serve the sibling manifest.yaml one level
    # up from gcs/: either rejected (400) or normalized to a miss (404),
    # never 200 with the parent file's bytes.
    r = client.get(f"/api/plugins/{pid}/gcs/%2e%2e/manifest.yaml")
    assert r.status_code in (400, 404), r.text
    assert "schema_version" not in r.text
