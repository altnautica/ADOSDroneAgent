"""Tests for the cloud-relay install receiver."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import httpx
import pytest

from ados.api.routes._plugins_helpers import read_sidecar
from ados.plugins import remote_install as rinstall
from ados.plugins import remote_install_download as rdl
from ados.plugins.errors import SignatureError, SupervisorError
from ados.plugins.remote_install import (
    DownloadError,
    RemoteInstallReceiver,
    already_seen,
    is_plugin_command,
    mark_seen,
)

# ---------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------


@pytest.fixture
def isolated_install_state(tmp_path: Path, monkeypatch):
    seen_path = tmp_path / "seen.json"
    sidecar_root = tmp_path / "run"
    sidecar_root.mkdir()
    grants_root = tmp_path / "grants"
    grants_root.mkdir()
    # Default seen_jobs_path used by helpers that read the module-level constant.
    monkeypatch.setattr(rinstall, "SEEN_JOBS_PATH", seen_path, raising=False)
    return {
        "seen_path": seen_path,
        "sidecar_root": sidecar_root,
        "grants_root": grants_root,
    }


@pytest.fixture
def supervisor_double():
    install_result = MagicMock()
    install_result.plugin_id = "com.example.plug"
    install_result.version = "0.1.0"
    install_result.signer_id = "first-party"
    install_result.risk = "low"
    install_result.permissions_requested = ["event.publish"]

    install_record = MagicMock()
    install_record.manifest_hash = "deadbeef"
    install_record.plugin_id = "com.example.plug"
    install_record.permissions = {}

    sup = MagicMock()
    sup.install_archive.return_value = install_result
    sup.find_install.return_value = install_record
    sup.grant_permission.return_value = None
    sup.remove.return_value = None
    sup.enable.return_value = None
    sup.disable.return_value = None
    return sup


def _make_cmd(job_id: str = "job-1", **overrides) -> dict:
    args = {
        "jobId": job_id,
        "pluginId": "com.example.plug",
        "operatorId": "op-42",
        "archiveId": "archive-1",
        # Convex deployment hostnames end in ``.convex.cloud``. The
        # allowlist accepts these by suffix match.
        "signedUrl": "https://example.convex.cloud/archive.adosplug",
        "requestedPermissions": ["event.publish"],
    }
    args.update(overrides)
    return {"_id": job_id, "command": "plugin.install", "args": args}


class _FakeResp:
    def __init__(self, status_code: int, content: bytes = b"", body: dict | None = None):
        self.status_code = status_code
        self.content = content
        self._body = body or {}

    def json(self):
        return self._body


# ---------------------------------------------------------------------
# is_plugin_command + idempotency primitives
# ---------------------------------------------------------------------


def test_is_plugin_command_lists_install_uninstall_enable_disable_configure():
    assert is_plugin_command("plugin.install")
    assert is_plugin_command("plugin.uninstall")
    assert is_plugin_command("plugin.enable")
    assert is_plugin_command("plugin.disable")
    assert is_plugin_command("plugin.configure")
    assert not is_plugin_command("get_services")


def test_idempotency_ring_round_trip(isolated_install_state):
    p = isolated_install_state["seen_path"]
    assert not already_seen("job-x", path=p)
    mark_seen("job-x", path=p)
    assert already_seen("job-x", path=p)


def test_idempotency_ring_caps_at_max(isolated_install_state, monkeypatch):
    p = isolated_install_state["seen_path"]
    monkeypatch.setattr(rinstall, "SEEN_JOBS_MAX", 10, raising=False)
    for i in range(15):
        mark_seen(f"job-{i}", path=p)
    raw = json.loads(p.read_text())
    # 10% drop on overflow keeps ~14 entries (drops 1).
    assert len(raw) <= 15


# ---------------------------------------------------------------------
# handle_install
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_handle_install_happy_path_writes_sidecar_and_acks(
    isolated_install_state, supervisor_double
):
    archive_bytes = b"PK\x03\x04" + b"\x00" * 32  # any zip-ish bytes; supervisor is mocked

    async def fake_handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=archive_bytes)

    client = httpx.AsyncClient(transport=httpx.MockTransport(fake_handler))
    try:
        status, result, data = await RemoteInstallReceiver.handle_install(
            _make_cmd(job_id="job-A"),
            supervisor=supervisor_double,
            device_id="device-007",
            api_key="K",
            convex_url="https://convex.invalid",
            sidecar_root=isolated_install_state["sidecar_root"],
            seen_jobs_path=isolated_install_state["seen_path"],
            grants_root=isolated_install_state["grants_root"],
            http_client=client,
        )
    finally:
        await client.aclose()

    assert status == "completed"
    assert result == {"success": True, "message": "installed"}
    assert data["installId"] == "job-A"
    assert data["pluginId"] == "com.example.plug"
    assert data["version"] == "0.1.0"
    assert data["manifestHash"] == "deadbeef"

    # Sidecar reflects the terminal stage.
    side = read_sidecar("job-A", root=isolated_install_state["sidecar_root"])
    assert side is not None
    assert side["stage"] == "completed"
    assert side["pluginId"] == "com.example.plug"

    # Idempotency ring records the job.
    assert already_seen("job-A", path=isolated_install_state["seen_path"])

    # Grant audit log exists.
    grant_file = (
        isolated_install_state["grants_root"]
        / "com.example.plug"
        / "granted-permissions.yaml"
    )
    assert grant_file.exists()
    text = grant_file.read_text()
    assert "operator_id" in text
    assert '"op-42"' in text
    assert '"device-007"' in text


@pytest.mark.asyncio
async def test_handle_install_replay_is_idempotent(
    isolated_install_state, supervisor_double
):
    mark_seen("job-B", path=isolated_install_state["seen_path"])
    status, result, data = await RemoteInstallReceiver.handle_install(
        _make_cmd(job_id="job-B"),
        supervisor=supervisor_double,
        device_id="device-007",
        sidecar_root=isolated_install_state["sidecar_root"],
        seen_jobs_path=isolated_install_state["seen_path"],
        grants_root=isolated_install_state["grants_root"],
    )
    assert status == "completed"
    assert data["replay"] is True
    supervisor_double.install_archive.assert_not_called()


@pytest.mark.asyncio
async def test_handle_install_refreshes_signed_url_on_401(
    isolated_install_state, supervisor_double, monkeypatch
):
    # Avoid the retry backoff slowing the test.
    monkeypatch.setattr(rinstall, "DOWNLOAD_RETRY_DELAYS", (0.0, 0.0, 0.0), raising=False)
    archive_bytes = b"PK\x03\x04" + b"\x00" * 8
    call_counter = {"n": 0}

    async def fake_handler(request: httpx.Request) -> httpx.Response:
        url = str(request.url)
        if url.endswith("/refreshDownload"):
            return httpx.Response(
                200,
                json={"signedUrl": "https://example.convex.cloud/fresh.adosplug"},
            )
        call_counter["n"] += 1
        if call_counter["n"] == 1:
            return httpx.Response(401)
        return httpx.Response(200, content=archive_bytes)

    client = httpx.AsyncClient(transport=httpx.MockTransport(fake_handler))
    try:
        status, _, _ = await RemoteInstallReceiver.handle_install(
            _make_cmd(job_id="job-401"),
            supervisor=supervisor_double,
            device_id="device-007",
            api_key="K",
            convex_url="https://convex.invalid",
            sidecar_root=isolated_install_state["sidecar_root"],
            seen_jobs_path=isolated_install_state["seen_path"],
            grants_root=isolated_install_state["grants_root"],
            http_client=client,
        )
    finally:
        await client.aclose()
    assert status == "completed"
    # First call 401, refresh URL, second call 200 — three transport calls total.
    assert call_counter["n"] >= 2


@pytest.mark.asyncio
async def test_handle_install_download_retries_then_fails(
    isolated_install_state, supervisor_double, monkeypatch
):
    monkeypatch.setattr(rinstall, "DOWNLOAD_RETRY_DELAYS", (0.0, 0.0, 0.0), raising=False)

    async def fake_handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(500)

    client = httpx.AsyncClient(transport=httpx.MockTransport(fake_handler))
    try:
        status, result, data = await RemoteInstallReceiver.handle_install(
            _make_cmd(job_id="job-500"),
            supervisor=supervisor_double,
            device_id="device-007",
            api_key="K",
            convex_url="https://convex.invalid",
            sidecar_root=isolated_install_state["sidecar_root"],
            seen_jobs_path=isolated_install_state["seen_path"],
            http_client=client,
        )
    finally:
        await client.aclose()
    assert status == "failed"
    assert data["code"] == "download_failed"
    side = read_sidecar("job-500", root=isolated_install_state["sidecar_root"])
    assert side is not None
    assert side["stage"] == "failed"


@pytest.mark.asyncio
async def test_handle_install_signature_error_classifies_to_signature_code(
    isolated_install_state, supervisor_double
):
    archive_bytes = b"PK\x03\x04"
    supervisor_double.install_archive.side_effect = SignatureError(
        SignatureError.KIND_INVALID, "bad sig"
    )

    async def fake_handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=archive_bytes)

    client = httpx.AsyncClient(transport=httpx.MockTransport(fake_handler))
    try:
        status, _, data = await RemoteInstallReceiver.handle_install(
            _make_cmd(job_id="job-sig"),
            supervisor=supervisor_double,
            device_id="device-007",
            sidecar_root=isolated_install_state["sidecar_root"],
            seen_jobs_path=isolated_install_state["seen_path"],
            http_client=client,
        )
    finally:
        await client.aclose()
    assert status == "failed"
    assert data["code"].startswith("signature_")


# ---------------------------------------------------------------------
# dispatch (non-install commands)
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_dispatch_uninstall(isolated_install_state, supervisor_double):
    cmd = {
        "_id": "job-U",
        "command": "plugin.uninstall",
        "args": {"jobId": "job-U", "pluginId": "com.example.plug", "keepData": False},
    }
    status, result, data = await RemoteInstallReceiver.dispatch(
        cmd,
        supervisor=supervisor_double,
        device_id="device-007",
        seen_jobs_path=isolated_install_state["seen_path"],
    )
    assert status == "completed"
    assert data["action"] == "uninstalled"
    supervisor_double.remove.assert_called_once_with("com.example.plug", keep_data=False)


@pytest.mark.asyncio
async def test_dispatch_enable_disable(isolated_install_state, supervisor_double):
    for cmd_name, expected_action, mock_attr in [
        ("plugin.enable", "enabled", supervisor_double.enable),
        ("plugin.disable", "disabled", supervisor_double.disable),
    ]:
        cmd = {
            "_id": f"j-{cmd_name}",
            "command": cmd_name,
            "args": {"jobId": f"j-{cmd_name}", "pluginId": "com.example.plug"},
        }
        status, _, data = await RemoteInstallReceiver.dispatch(
            cmd,
            supervisor=supervisor_double,
            device_id="device-007",
            seen_jobs_path=isolated_install_state["seen_path"],
        )
        assert status == "completed"
        assert data["action"] == expected_action
        mock_attr.assert_called()


@pytest.mark.asyncio
async def test_dispatch_supervisor_error_surfaces_failure(
    isolated_install_state, supervisor_double
):
    supervisor_double.enable.side_effect = SupervisorError("not installed")
    cmd = {
        "_id": "j-err",
        "command": "plugin.enable",
        "args": {"jobId": "j-err", "pluginId": "missing"},
    }
    status, result, data = await RemoteInstallReceiver.dispatch(
        cmd,
        supervisor=supervisor_double,
        device_id="device-007",
        seen_jobs_path=isolated_install_state["seen_path"],
    )
    assert status == "failed"
    assert data["code"] == "supervisor_error"


@pytest.mark.asyncio
async def test_dispatch_unknown_command(isolated_install_state, supervisor_double):
    cmd = {
        "_id": "j-unk",
        "command": "plugin.bogus",
        "args": {"jobId": "j-unk", "pluginId": "com.example.plug"},
    }
    status, result, _ = await RemoteInstallReceiver.dispatch(
        cmd,
        supervisor=supervisor_double,
        device_id="device-007",
        seen_jobs_path=isolated_install_state["seen_path"],
    )
    assert status == "failed"
    assert "unknown" in result["message"].lower()


# ---------------------------------------------------------------------
# Sidecar write resilience
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_sidecar_records_progress_across_stages(
    isolated_install_state, supervisor_double
):
    archive_bytes = b"PK\x03\x04"

    captured_stages: list[str] = []
    original_write = rinstall.write_sidecar

    def tracking_write(job_id, payload, *, root=None):
        captured_stages.append(payload.get("stage"))
        return original_write(job_id, payload, root=root)

    async def fake_handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=archive_bytes)

    client = httpx.AsyncClient(transport=httpx.MockTransport(fake_handler))
    try:
        with patch.object(rinstall, "write_sidecar", side_effect=tracking_write):
            await RemoteInstallReceiver.handle_install(
                _make_cmd(job_id="job-S"),
                supervisor=supervisor_double,
                device_id="device-007",
                api_key="K",
                convex_url="https://convex.invalid",
                sidecar_root=isolated_install_state["sidecar_root"],
                seen_jobs_path=isolated_install_state["seen_path"],
                grants_root=isolated_install_state["grants_root"],
                http_client=client,
            )
    finally:
        await client.aclose()

    # We should see queued → downloading → verifying → installing → enabling → completed
    assert "queued" in captured_stages
    assert "downloading" in captured_stages
    assert "completed" in captured_stages


# ---------------------------------------------------------------------
# Download defenses: URL scheme, host allowlist, size cap, SHA256
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_download_rejects_http_scheme(
    isolated_install_state, supervisor_double, monkeypatch
):
    monkeypatch.setattr(
        rinstall, "DOWNLOAD_RETRY_DELAYS", (0.0, 0.0, 0.0), raising=False
    )
    cmd = _make_cmd(job_id="job-http")
    cmd["args"]["signedUrl"] = "http://example.convex.cloud/archive.adosplug"

    status, _, data = await RemoteInstallReceiver.handle_install(
        cmd,
        supervisor=supervisor_double,
        device_id="device-007",
        api_key="K",
        convex_url="https://convex.invalid",
        sidecar_root=isolated_install_state["sidecar_root"],
        seen_jobs_path=isolated_install_state["seen_path"],
    )
    assert status == "failed"
    assert data["code"] == "download_failed"
    side = read_sidecar("job-http", root=isolated_install_state["sidecar_root"])
    assert side is not None
    assert side["stage"] == "failed"
    assert "non-https" in (side.get("detail") or "")
    # Supervisor must not have been touched — defense fired before download.
    supervisor_double.install_archive.assert_not_called()


@pytest.mark.asyncio
async def test_download_rejects_host_outside_allowlist(
    isolated_install_state, supervisor_double, monkeypatch
):
    monkeypatch.setattr(
        rinstall, "DOWNLOAD_RETRY_DELAYS", (0.0, 0.0, 0.0), raising=False
    )
    cmd = _make_cmd(job_id="job-host")
    cmd["args"]["signedUrl"] = "https://evil.example.com/archive.adosplug"

    status, _, data = await RemoteInstallReceiver.handle_install(
        cmd,
        supervisor=supervisor_double,
        device_id="device-007",
        api_key="K",
        convex_url="https://convex.invalid",
        sidecar_root=isolated_install_state["sidecar_root"],
        seen_jobs_path=isolated_install_state["seen_path"],
    )
    assert status == "failed"
    assert data["code"] == "download_failed"
    side = read_sidecar("job-host", root=isolated_install_state["sidecar_root"])
    assert side is not None
    assert "allowlist" in (side.get("detail") or "")
    supervisor_double.install_archive.assert_not_called()


@pytest.mark.asyncio
async def test_download_size_cap_aborts_stream(
    isolated_install_state, supervisor_double, monkeypatch
):
    """A 10 GB body must not buffer; the streamer aborts at the cap."""
    monkeypatch.setattr(
        rinstall, "DOWNLOAD_RETRY_DELAYS", (0.0, 0.0, 0.0), raising=False
    )
    # Shrink the cap so the test does not actually emit megabytes.
    monkeypatch.setattr(rdl, "DOWNLOAD_MAX_BYTES", 1024, raising=False)

    chunk = b"A" * 256

    async def fake_handler(request: httpx.Request) -> httpx.Response:
        # 8 chunks * 256 bytes = 2048 bytes, well past the 1024 cap.
        async def gen():
            for _ in range(8):
                yield chunk
        return httpx.Response(200, content=gen())

    client = httpx.AsyncClient(transport=httpx.MockTransport(fake_handler))
    try:
        cmd = _make_cmd(job_id="job-big")
        cmd["args"]["signedUrl"] = "https://abc.convex.cloud/big.adosplug"
        status, _, data = await RemoteInstallReceiver.handle_install(
            cmd,
            supervisor=supervisor_double,
            device_id="device-007",
            api_key="K",
            convex_url="https://convex.invalid",
            sidecar_root=isolated_install_state["sidecar_root"],
            seen_jobs_path=isolated_install_state["seen_path"],
            http_client=client,
        )
    finally:
        await client.aclose()

    assert status == "failed"
    assert data["code"] == "download_failed"
    side = read_sidecar("job-big", root=isolated_install_state["sidecar_root"])
    assert side is not None
    assert "size cap" in (side.get("detail") or "")
    supervisor_double.install_archive.assert_not_called()


@pytest.mark.asyncio
async def test_download_sha256_mismatch_rejects(
    isolated_install_state, supervisor_double, monkeypatch
):
    monkeypatch.setattr(
        rinstall, "DOWNLOAD_RETRY_DELAYS", (0.0, 0.0, 0.0), raising=False
    )
    archive_bytes = b"PK\x03\x04 actual contents"
    real_sha = hashlib.sha256(archive_bytes).hexdigest()
    assert real_sha != "deadbeef" * 8  # sanity

    async def fake_handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=archive_bytes)

    client = httpx.AsyncClient(transport=httpx.MockTransport(fake_handler))
    try:
        cmd = _make_cmd(job_id="job-sha")
        cmd["args"]["signedUrl"] = "https://abc.convex.cloud/x.adosplug"
        # Intentionally wrong hash; defense-in-depth fires before
        # supervisor sees the bytes.
        cmd["args"]["manifestHash"] = "deadbeef" * 8
        status, _, data = await RemoteInstallReceiver.handle_install(
            cmd,
            supervisor=supervisor_double,
            device_id="device-007",
            api_key="K",
            convex_url="https://convex.invalid",
            sidecar_root=isolated_install_state["sidecar_root"],
            seen_jobs_path=isolated_install_state["seen_path"],
            http_client=client,
        )
    finally:
        await client.aclose()

    assert status == "failed"
    assert data["code"] == "download_failed"
    side = read_sidecar("job-sha", root=isolated_install_state["sidecar_root"])
    assert side is not None
    assert "sha256" in (side.get("detail") or "")
    supervisor_double.install_archive.assert_not_called()


# ---------------------------------------------------------------------
# Unit tests on the pure validator (no httpx client needed)
# ---------------------------------------------------------------------


def test_validate_download_url_accepts_convex_cloud():
    rdl.validate_download_url("https://abc.convex.cloud/x.adosplug")


def test_validate_download_url_accepts_convex_altnautica():
    rdl.validate_download_url(
        "https://xyz.convex.altnautica.com/path/to/archive.adosplug"
    )


def test_validate_download_url_accepts_localhost_for_dev():
    rdl.validate_download_url("https://localhost:8443/x.adosplug")


def test_validate_download_url_rejects_substring_match():
    """``evil.localhost.example.com`` must NOT match ``localhost``."""
    with pytest.raises(DownloadError):
        rdl.validate_download_url("https://evil.localhost.example.com/x")


def test_validate_download_url_rejects_empty():
    with pytest.raises(DownloadError):
        rdl.validate_download_url("")
