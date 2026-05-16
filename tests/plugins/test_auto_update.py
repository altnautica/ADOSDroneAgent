"""Tests for the plugin auto-update daily poll.

Covers the decision tree (silent install, notify, skipped, failed),
the daily-loop scaffolding (jitter sleep, shutdown handling, state
persistence), and the failure paths that record the last attempt on
the install record.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest

from ados.plugins import auto_update as au
from ados.plugins.auto_update import (
    AutoUpdateOutcome,
    check_one_plugin,
    latest_check_timestamp_ms,
    run_daily_loop,
)
from ados.plugins.errors import SignatureError, SupervisorError
from ados.plugins.state import (
    PermissionGrant,
    PluginInstall,
    load_state,
    save_state,
)


# ---------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------


@pytest.fixture
def isolated_state(tmp_path: Path, monkeypatch):
    """Redirect plugin state.json to a per-test file."""
    state_path = tmp_path / "plugin-state.json"
    monkeypatch.setattr(
        "ados.plugins.state.PLUGIN_STATE_PATH", state_path, raising=True
    )
    return state_path


def _make_install(
    plugin_id: str = "com.example.plug",
    version: str = "1.0.0",
    *,
    auto_update: bool = True,
    pinned_version: str | None = None,
    status: str = "enabled",
    permissions: dict | None = None,
) -> PluginInstall:
    perms = permissions or {}
    return PluginInstall(
        plugin_id=plugin_id,
        version=version,
        source="registry",
        source_uri=None,
        signer_id="first-party",
        manifest_hash="abc123",
        status=status,
        installed_at=1_700_000_000_000,
        permissions=perms,
        auto_update=auto_update,
        pinned_version=pinned_version,
    )


def _make_supervisor(install: PluginInstall) -> MagicMock:
    sup = MagicMock()
    sup.installs.return_value = [install]
    sup.find_install.return_value = install
    sup.disable.return_value = None
    sup.remove.return_value = None
    sup.enable.return_value = None
    sup.grant_permission.return_value = None
    sup.install_archive.return_value = MagicMock(
        plugin_id=install.plugin_id, version="1.1.0"
    )
    return sup


def _manifest_yaml(
    *,
    plugin_id: str = "com.example.plug",
    version: str = "1.1.0",
    permissions: list | None = None,
) -> str:
    perms = permissions if permissions is not None else ["event.publish"]
    perms_yaml = "\n".join(f"      - {p}" for p in perms)
    return f"""
id: {plugin_id}
version: {version}
name: Example
description: ""
license: GPL-3.0
risk: low
compatibility:
  ados_version: ">=0.0.0"
  supported_boards: []
agent:
  entrypoint: example:Plugin
  isolation: subprocess
  permissions:
{perms_yaml}
""".strip()


def _registry_payload(
    *,
    latest_version: str = "1.1.0",
    permissions: list | None = None,
    supported_boards: list | None = None,
    download_url: str = "https://example.convex.cloud/archive.adosplug",
    archive_sha256: str = "",
) -> dict:
    return {
        "plugin": {"plugin_id": "com.example.plug"},
        "versions": [
            {
                "version": latest_version,
                "manifest_yaml": _manifest_yaml(
                    version=latest_version, permissions=permissions
                ),
                "download_url": download_url,
                "archive_sha256": archive_sha256,
                "supported_boards": supported_boards or [],
                "released_at": 1_700_000_000_000,
            }
        ],
    }


class _StubResp:
    def __init__(self, status_code: int, json_body: dict):
        self.status_code = status_code
        self._body = json_body
        self.text = ""

    def json(self):
        return self._body


def _stub_client(payload: dict, *, status_code: int = 200) -> MagicMock:
    """An AsyncClient double whose ``post`` returns a stub envelope."""
    client = MagicMock()
    envelope = {"status": "success", "value": payload}

    async def _post(url, json=None, headers=None):
        return _StubResp(status_code, envelope)

    client.post = AsyncMock(side_effect=_post)
    return client


# ---------------------------------------------------------------------
# Semver helpers
# ---------------------------------------------------------------------


def test_is_newer_basic():
    assert au._is_newer("1.1.0", "1.0.0") is True
    assert au._is_newer("1.0.1", "1.0.0") is True
    assert au._is_newer("2.0.0", "1.9.9") is True
    assert au._is_newer("1.0.0", "1.0.0") is False
    assert au._is_newer("1.0.0", "1.0.1") is False


def test_is_major_bump():
    assert au._is_major_bump("2.0.0", "1.9.9") is True
    assert au._is_major_bump("1.5.0", "1.4.0") is False
    assert au._is_major_bump("1.0.1", "1.0.0") is False


def test_parse_semver_strips_prerelease():
    assert au._parse_semver("1.2.3-rc.1") == (1, 2, 3)
    assert au._parse_semver("1.2.3+build.5") == (1, 2, 3)


# ---------------------------------------------------------------------
# Decision tree
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_skipped_when_pinned(isolated_state):
    install = _make_install(pinned_version="1.0.0")
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(_registry_payload())

    outcome = await check_one_plugin(
        install=install,
        supervisor=sup,
        http_client=client,
        convex_url="https://example.convex.cloud",
        api_key="key",
        device_id="dev-1",
        current_board_id="rpi4b",
    )

    assert outcome is AutoUpdateOutcome.SKIPPED
    # Pinned plugins never hit the registry.
    client.post.assert_not_called()


@pytest.mark.asyncio
async def test_skipped_when_auto_update_off(isolated_state):
    install = _make_install(auto_update=False)
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(_registry_payload())

    outcome = await check_one_plugin(
        install=install,
        supervisor=sup,
        http_client=client,
        convex_url="https://example.convex.cloud",
        api_key="key",
        device_id="dev-1",
        current_board_id="rpi4b",
    )

    assert outcome is AutoUpdateOutcome.SKIPPED


@pytest.mark.asyncio
async def test_skipped_when_no_newer_version(isolated_state):
    install = _make_install(version="1.1.0")
    save_state([install])
    sup = _make_supervisor(install)
    # Registry's latest is 1.1.0 too.
    client = _stub_client(_registry_payload(latest_version="1.1.0"))

    outcome = await check_one_plugin(
        install=install,
        supervisor=sup,
        http_client=client,
        convex_url="https://example.convex.cloud",
        api_key="key",
        device_id="dev-1",
        current_board_id="rpi4b",
    )

    assert outcome is AutoUpdateOutcome.SKIPPED


@pytest.mark.asyncio
async def test_notify_on_major_bump(isolated_state):
    install = _make_install(version="1.5.0")
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(_registry_payload(latest_version="2.0.0"))

    with patch.object(au, "_publish_update_notice") as pub:
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )

    assert outcome is AutoUpdateOutcome.NOTIFY
    pub.assert_called_once()
    notice = pub.call_args[0][1]
    assert notice["reason"] == "major_bump"
    assert notice["latest_version"] == "2.0.0"


@pytest.mark.asyncio
async def test_notify_on_permission_delta(isolated_state):
    install = _make_install(
        version="1.0.0",
        permissions={
            "event.publish": PermissionGrant(granted=True, granted_at=1)
        },
    )
    save_state([install])
    sup = _make_supervisor(install)
    # New version requires event.publish AND hardware.spi.
    client = _stub_client(
        _registry_payload(
            latest_version="1.1.0",
            permissions=["event.publish", "hardware.spi"],
        )
    )

    with patch.object(au, "_publish_update_notice") as pub:
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )

    assert outcome is AutoUpdateOutcome.NOTIFY
    notice = pub.call_args[0][1]
    assert notice["reason"] == "permission_delta"
    assert notice["new_permissions"] == ["hardware.spi"]


@pytest.mark.asyncio
async def test_notify_on_board_mismatch(isolated_state):
    install = _make_install()
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(
        _registry_payload(
            latest_version="1.1.0",
            supported_boards=["rpi5", "rock-5c-lite"],
        )
    )

    with patch.object(au, "_publish_update_notice") as pub:
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )

    assert outcome is AutoUpdateOutcome.NOTIFY
    notice = pub.call_args[0][1]
    assert notice["reason"] == "board_mismatch"


@pytest.mark.asyncio
async def test_silent_install_happy_path(isolated_state):
    install = _make_install(
        version="1.0.0",
        permissions={
            "event.publish": PermissionGrant(granted=True, granted_at=1)
        },
    )
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(
        _registry_payload(latest_version="1.1.0")
    )

    # Stub the download path so we don't hit the network.
    async def _fake_stream(http, url):
        return 200, b"archive-bytes"

    with patch.object(au, "stream_download", side_effect=_fake_stream), patch.object(
        au, "validate_download_url"
    ), patch.object(au, "verify_sha256"):
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )

    assert outcome is AutoUpdateOutcome.SILENT_INSTALL
    # Sequence: disable → remove → install_archive → grant → enable.
    sup.disable.assert_called_once_with("com.example.plug")
    sup.remove.assert_called_once_with("com.example.plug", keep_data=False)
    sup.install_archive.assert_called_once()
    sup.grant_permission.assert_called_once_with(
        "com.example.plug", "event.publish"
    )
    sup.enable.assert_called_once_with("com.example.plug")


@pytest.mark.asyncio
async def test_silent_install_signature_failure_records_attempt(isolated_state):
    # Pre-grant event.publish so the remote manifest's default permission
    # set matches and we land on the silent-install path (not notify).
    install = _make_install(
        permissions={
            "event.publish": PermissionGrant(granted=True, granted_at=1)
        }
    )
    save_state([install])
    sup = _make_supervisor(install)
    sup.install_archive.side_effect = SignatureError(
        SignatureError.KIND_INVALID, "bad sig"
    )
    client = _stub_client(_registry_payload(latest_version="1.1.0"))

    async def _fake_stream(http, url):
        return 200, b"archive-bytes"

    with patch.object(au, "stream_download", side_effect=_fake_stream), patch.object(
        au, "validate_download_url"
    ), patch.object(au, "verify_sha256"):
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )

    assert outcome is AutoUpdateOutcome.FAILED
    assert install.last_update_attempt is not None
    assert install.last_update_attempt["outcome"] == "failed"
    assert "bad sig" in install.last_update_attempt["error"]


@pytest.mark.asyncio
async def test_silent_install_download_failure_records_attempt(isolated_state):
    # Match permissions so the path reaches the download step (not notify).
    install = _make_install(
        permissions={
            "event.publish": PermissionGrant(granted=True, granted_at=1)
        }
    )
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(_registry_payload(latest_version="1.1.0"))

    async def _bad_stream(http, url):
        raise httpx.ConnectError("dns fail")

    with patch.object(au, "stream_download", side_effect=_bad_stream), patch.object(
        au, "validate_download_url"
    ):
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )

    assert outcome is AutoUpdateOutcome.FAILED
    assert install.last_update_attempt is not None
    assert install.last_update_attempt["outcome"] == "failed"
    assert "download" in install.last_update_attempt["error"]
    # The install was never disabled (download failed before swap).
    sup.disable.assert_not_called()


@pytest.mark.asyncio
async def test_registry_query_failure_records_attempt(isolated_state):
    install = _make_install()
    save_state([install])
    sup = _make_supervisor(install)
    # Network error talking to the registry.
    client = MagicMock()

    async def _post(url, json=None, headers=None):
        raise httpx.ConnectError("dns fail")

    client.post = AsyncMock(side_effect=_post)

    outcome = await check_one_plugin(
        install=install,
        supervisor=sup,
        http_client=client,
        convex_url="https://example.convex.cloud",
        api_key="key",
        device_id="dev-1",
        current_board_id="rpi4b",
    )

    assert outcome is AutoUpdateOutcome.FAILED
    assert install.last_update_attempt["outcome"] == "failed"


# ---------------------------------------------------------------------
# Set-equality on permission delta
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_permission_added_triggers_notify(isolated_state):
    """{a,b} → {a,b,c} adds c — must notify."""
    install = _make_install(
        permissions={
            "event.publish": PermissionGrant(True, 1),
            "vehicle.status.read": PermissionGrant(True, 1),
        }
    )
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(
        _registry_payload(
            permissions=[
                "event.publish",
                "vehicle.status.read",
                "hardware.spi",
            ]
        )
    )
    with patch.object(au, "_publish_update_notice") as pub:
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )
    assert outcome is AutoUpdateOutcome.NOTIFY
    assert pub.call_args[0][1]["new_permissions"] == ["hardware.spi"]


@pytest.mark.asyncio
async def test_permission_swapped_triggers_notify(isolated_state):
    """{a,b} → {a,c} drops b and adds c — must notify on the addition."""
    install = _make_install(
        permissions={
            "event.publish": PermissionGrant(True, 1),
            "vehicle.status.read": PermissionGrant(True, 1),
        }
    )
    save_state([install])
    sup = _make_supervisor(install)
    client = _stub_client(
        _registry_payload(permissions=["event.publish", "hardware.spi"])
    )
    with patch.object(au, "_publish_update_notice") as pub:
        outcome = await check_one_plugin(
            install=install,
            supervisor=sup,
            http_client=client,
            convex_url="https://example.convex.cloud",
            api_key="key",
            device_id="dev-1",
            current_board_id="rpi4b",
        )
    assert outcome is AutoUpdateOutcome.NOTIFY
    assert pub.call_args[0][1]["new_permissions"] == ["hardware.spi"]


# ---------------------------------------------------------------------
# Daily loop scaffolding
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_daily_loop_iterates_and_records_checks(isolated_state):
    """A single cycle must stamp last_update_check_at on every install."""
    install = _make_install()
    save_state([install])

    ctx = MagicMock()
    ctx.config.agent.device_id = "dev-1"
    ctx.convex_url = "https://example.convex.cloud"
    ctx.pairing.is_paired = True
    ctx.pairing.api_key = "key"
    ctx.board.name = "rpi4b"
    shutdown = asyncio.Event()
    ctx.shutdown = shutdown

    # First sleep returns immediately; second sleep fires shutdown.
    sleep_calls = []

    async def _stub_sleep(seconds, evt):
        sleep_calls.append(seconds)
        if len(sleep_calls) >= 2:
            evt.set()
            return True
        return False

    fake_outcome = AutoUpdateOutcome.SKIPPED

    async def _fake_check(**kwargs):
        return fake_outcome

    # Patch the supervisor constructor so discover() doesn't try to
    # touch a real systemd / filesystem.
    sup = _make_supervisor(install)
    with patch.object(au, "PluginSupervisor", return_value=sup), patch.object(
        au, "_sleep_with_shutdown", side_effect=_stub_sleep
    ), patch.object(au, "check_one_plugin", side_effect=_fake_check):
        await run_daily_loop(ctx)

    # The loop slept twice: 60 s preroll + the daily jittered interval.
    assert len(sleep_calls) == 2
    assert sleep_calls[0] == 60.0
    # Last-check timestamp should be on the persisted install.
    persisted = load_state()
    assert len(persisted) == 1
    assert persisted[0].last_update_check_at is not None


@pytest.mark.asyncio
async def test_run_daily_loop_skips_unpaired(isolated_state):
    """When unpaired the loop sleeps but does not call the registry."""
    install = _make_install()
    save_state([install])

    ctx = MagicMock()
    ctx.config.agent.device_id = "dev-1"
    ctx.convex_url = ""
    ctx.pairing.is_paired = False
    ctx.pairing.api_key = None
    ctx.board.name = "rpi4b"
    shutdown = asyncio.Event()
    ctx.shutdown = shutdown

    call_count = {"n": 0}

    async def _stub_sleep(seconds, evt):
        call_count["n"] += 1
        if call_count["n"] >= 2:
            evt.set()
            return True
        return False

    with patch.object(au, "PluginSupervisor", return_value=_make_supervisor(install)), patch.object(
        au, "_sleep_with_shutdown", side_effect=_stub_sleep
    ), patch.object(au, "check_one_plugin") as check:
        await run_daily_loop(ctx)

    check.assert_not_called()


def test_next_sleep_seconds_jitter_range():
    """Daily sleep stays within +/- one hour of the canonical 24 h."""
    for _ in range(50):
        s = au._next_sleep_seconds()
        assert 23 * 3600 <= s <= 25 * 3600


def test_latest_check_timestamp_ms_aggregates_max(isolated_state):
    a = _make_install(plugin_id="com.example.a")
    a.last_update_check_at = 100
    b = _make_install(plugin_id="com.example.b")
    b.last_update_check_at = 500
    c = _make_install(plugin_id="com.example.c")
    c.last_update_check_at = None
    save_state([a, b, c])

    assert latest_check_timestamp_ms() == 500


def test_latest_check_timestamp_ms_returns_none_when_empty(isolated_state):
    install = _make_install()
    save_state([install])
    assert latest_check_timestamp_ms() is None
