"""Tests for ``POST /api/plugins/install_from_url``.

The endpoint downloads a ``.adosplug`` archive from an allowlisted
URL, stages it on disk, and delegates to the same supervisor flow as
the multipart ``/install`` endpoint. The fixtures here build a real
supervisor against ``tmp_path`` (no ``/var/ados``, no real
systemctl), the same way ``test_api_plugins.py`` does, and patch the
``httpx.AsyncClient.stream`` call to feed the route a known archive
body without going to the network.
"""

from __future__ import annotations

import hashlib
import zipfile
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any, AsyncIterator
from unittest.mock import MagicMock, patch

import httpx
import pytest
from fastapi.testclient import TestClient

from ados.api.routes.plugins import _set_supervisor_for_tests
from ados.api.server import create_app
from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.supervisor import PluginSupervisor
from tests.api_runtime_utils import build_api_runtime


# ---------------------------------------------------------------------
# Fixtures
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


def _build_archive(tmp_path: Path, plugin_id: str = "com.example.url") -> Path:
    manifest_yaml = f"""\
schema_version: 1
id: {plugin_id}
version: 0.1.0
name: From URL
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
# httpx.AsyncClient.stream patching helper
# ---------------------------------------------------------------------


def _patch_stream(
    *,
    status_code: int = 200,
    body: bytes = b"",
    raise_on_open: Exception | None = None,
    chunk_size: int = 65536,
):
    """Return a context manager that patches ``httpx.AsyncClient.stream``.

    The patched method returns an async context manager whose response
    object exposes ``status_code`` and ``aiter_bytes()`` over the
    supplied ``body``. If ``raise_on_open`` is set it is raised the
    moment the route enters the stream context, simulating a transport
    failure.
    """

    @asynccontextmanager
    async def fake_stream(
        self: httpx.AsyncClient,
        method: str,
        url: str,
        **kwargs: Any,
    ) -> AsyncIterator[Any]:
        if raise_on_open is not None:
            raise raise_on_open
        resp = MagicMock()
        resp.status_code = status_code

        async def aiter_bytes() -> AsyncIterator[bytes]:
            if status_code != 200 or not body:
                return
            for i in range(0, len(body), chunk_size):
                yield body[i : i + chunk_size]

        async def aread() -> bytes:
            return b""

        resp.aiter_bytes = aiter_bytes
        resp.aread = aread
        yield resp

    return patch.object(httpx.AsyncClient, "stream", fake_stream)


# ---------------------------------------------------------------------
# Happy path
# ---------------------------------------------------------------------


def test_install_from_url_happy_path(client, supervisor, tmp_path: Path):
    archive_path = _build_archive(tmp_path)
    raw = archive_path.read_bytes()
    sha = hashlib.sha256(raw).hexdigest()
    url = (
        "https://github.com/altnautica/ADOSExtensions/releases/download/"
        "extensions/vision-nav-v0.2.3/com.example.url-0.1.0.adosplug"
    )

    with _patch_stream(status_code=200, body=raw), patch(
        "ados.plugins.supervisor.subprocess.run"
    ) as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        resp = client.post(
            "/api/plugins/install_from_url",
            json={
                "url": url,
                "expected_sha256": sha,
                "requested_permissions": ["event.publish"],
            },
        )

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["ok"] is True
    assert body["plugin_id"] == "com.example.url"
    assert body["risk"] == "low"
    assert body["sha256"] == sha
    assert body["granted"] == ["event.publish"]

    listing = client.get("/api/plugins").json()
    assert any(i["plugin_id"] == "com.example.url" for i in listing["installs"])


def test_install_from_url_no_sha_pin_still_installs(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(tmp_path, plugin_id="com.example.nopin")
    raw = archive_path.read_bytes()
    url = "https://objects.githubusercontent.com/example/com.example.nopin.adosplug"

    with _patch_stream(status_code=200, body=raw), patch(
        "ados.plugins.supervisor.subprocess.run"
    ) as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        resp = client.post(
            "/api/plugins/install_from_url",
            json={"url": url},
        )

    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["ok"] is True
    assert body["plugin_id"] == "com.example.nopin"


def test_install_from_url_catalog_requires_sha(client, supervisor):
    """Catalog-sourced installs MUST pin a SHA — the route rejects the
    call before any download attempt when ``from_catalog=true`` and
    ``expected_sha256`` is missing or blank.
    """
    url = "https://objects.githubusercontent.com/example/com.example.catalog.adosplug"
    resp = client.post(
        "/api/plugins/install_from_url",
        json={"url": url, "from_catalog": True},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "sha256_required"


def test_install_from_url_catalog_blank_sha_rejected(client, supervisor):
    """Whitespace-only SHA strings are stripped before the catalog gate."""
    url = "https://objects.githubusercontent.com/example/com.example.catalog.adosplug"
    resp = client.post(
        "/api/plugins/install_from_url",
        json={"url": url, "from_catalog": True, "expected_sha256": "   "},
    )
    assert resp.status_code == 400
    assert resp.json()["kind"] == "sha256_required"


def test_install_from_url_catalog_json_null_sha_rejected(client, supervisor):
    """Explicit JSON null for ``expected_sha256`` also triggers the
    catalog gate. Pydantic coerces ``null`` to ``None`` and the
    handler treats None and missing identically — both must fail.
    """
    url = "https://objects.githubusercontent.com/example/com.example.catalog.adosplug"
    resp = client.post(
        "/api/plugins/install_from_url",
        json={
            "url": url,
            "from_catalog": True,
            "expected_sha256": None,
        },
    )
    assert resp.status_code == 400
    assert resp.json()["kind"] == "sha256_required"


def test_install_from_url_catalog_with_sha_passes_gate(
    client, supervisor, tmp_path: Path
):
    """A catalog install that ships a pinned SHA clears the gate and
    proceeds to download + verify + install like any other call.
    """
    archive_path = _build_archive(tmp_path, plugin_id="com.example.catalog2")
    raw = archive_path.read_bytes()
    sha = hashlib.sha256(raw).hexdigest()
    url = "https://objects.githubusercontent.com/example/com.example.catalog2.adosplug"

    with _patch_stream(status_code=200, body=raw), patch(
        "ados.plugins.supervisor.subprocess.run"
    ) as run_mock:
        run_mock.return_value = MagicMock(returncode=0, stderr="")
        resp = client.post(
            "/api/plugins/install_from_url",
            json={
                "url": url,
                "expected_sha256": sha,
                "from_catalog": True,
            },
        )

    assert resp.status_code == 200, resp.text
    assert resp.json()["plugin_id"] == "com.example.catalog2"


# ---------------------------------------------------------------------
# URL validation
# ---------------------------------------------------------------------


def test_install_from_url_rejects_http_scheme(client, supervisor):
    resp = client.post(
        "/api/plugins/install_from_url",
        json={"url": "http://github.com/foo/bar.adosplug"},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "url_invalid"
    assert "https" in body["detail"].lower()


def test_install_from_url_rejects_unknown_host(client, supervisor):
    resp = client.post(
        "/api/plugins/install_from_url",
        json={"url": "https://evil.example.org/x.adosplug"},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "url_invalid"
    assert "allowlist" in body["detail"].lower()


def test_install_from_url_rejects_empty_url(client, supervisor):
    resp = client.post(
        "/api/plugins/install_from_url",
        json={"url": ""},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "usage_error"


def test_install_from_url_rejects_suffix_lookalike(client, supervisor):
    # ``evil-github.com`` ends with ``github.com`` as a substring but
    # not at a labelled boundary, so the suffix check must reject it.
    resp = client.post(
        "/api/plugins/install_from_url",
        json={"url": "https://evil-github.com/x.adosplug"},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["kind"] == "url_invalid"


# ---------------------------------------------------------------------
# Integrity + transport failures
# ---------------------------------------------------------------------


def test_install_from_url_sha_mismatch_returns_400(
    client, supervisor, tmp_path: Path
):
    archive_path = _build_archive(tmp_path, plugin_id="com.example.mismatch")
    raw = archive_path.read_bytes()
    url = "https://github.com/altnautica/ADOSExtensions/releases/download/x/y.adosplug"
    bogus_sha = "deadbeef" + "00" * 28

    with _patch_stream(status_code=200, body=raw):
        resp = client.post(
            "/api/plugins/install_from_url",
            json={"url": url, "expected_sha256": bogus_sha},
        )
    assert resp.status_code == 400
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "sha256_mismatch"
    # The response intentionally does NOT echo the computed digest of
    # the served bytes — that would be a fingerprinting oracle. A
    # static "pin did not match" suffices.
    assert "pin" in body["detail"].lower()
    real_sha = hashlib.sha256(raw).hexdigest()
    assert real_sha not in body["detail"]


def test_install_from_url_download_failure_returns_502(client, supervisor):
    url = "https://github.com/foo/bar.adosplug"
    with _patch_stream(
        raise_on_open=httpx.ConnectError("connection refused")
    ):
        resp = client.post(
            "/api/plugins/install_from_url",
            json={"url": url},
        )
    assert resp.status_code == 502
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "download_failed"


def test_install_from_url_non_200_returns_502(client, supervisor):
    url = "https://github.com/foo/bar.adosplug"
    with _patch_stream(status_code=404, body=b""):
        resp = client.post(
            "/api/plugins/install_from_url",
            json={"url": url},
        )
    assert resp.status_code == 502
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "download_failed"
    assert "404" in body["detail"]


def test_install_from_url_oversized_returns_413(
    client, supervisor, monkeypatch
):
    """Patch the size cap down so a small body still trips the guard."""
    import ados.plugins.install_from_url_impl as impl

    monkeypatch.setattr(impl, "MAX_PLUGIN_ARCHIVE_SIZE", 128, raising=False)

    big_body = b"\x00" * 1024  # well over 128 bytes
    url = "https://github.com/foo/big.adosplug"
    with _patch_stream(status_code=200, body=big_body):
        resp = client.post(
            "/api/plugins/install_from_url",
            json={"url": url},
        )
    assert resp.status_code == 413
    body = resp.json()
    assert body["ok"] is False
    assert body["kind"] == "archive_too_large"
