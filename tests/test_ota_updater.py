"""Tests for OTA update orchestrator."""

from __future__ import annotations

from unittest.mock import AsyncMock, patch

import pytest
from ados.services.ota.rollback import RollbackManager

from ados.core.config import OtaConfig
from ados.services.ota.checker import UpdateChecker
from ados.services.ota.downloader import UpdateDownloader
from ados.services.ota.manifest import UpdateManifest
from ados.services.ota.updater import OtaUpdater, UpdateState


def _make_manifest(**overrides) -> UpdateManifest:
    data = {
        "version": "0.2.0",
        "channel": "stable",
        "release_date": "2026-03-08T00:00:00Z",
        "download_url": "https://updates.altnautica.com/stable/ados-0.2.0.bin",
        "file_size": 1024,
        "sha256": "a" * 64,
        "signature": "c2ln",
        "min_version": "0.1.0",
        "changelog": "Fixes.",
        "requires_reboot": False,
    }
    data.update(overrides)
    return UpdateManifest(**data)


@pytest.fixture
def updater(tmp_path):
    config = OtaConfig()
    checker = UpdateChecker(config)
    downloader = UpdateDownloader()
    rollback = RollbackManager(status_path=str(tmp_path / "boot-status.json"))
    return OtaUpdater(
        config=config,
        checker=checker,
        downloader=downloader,
        rollback=rollback,
        current_version="0.1.0",
    )


@pytest.mark.asyncio
async def test_check_finds_update(updater):
    manifest = _make_manifest()

    with patch.object(
        updater._checker, "check_for_update", new_callable=AsyncMock, return_value=manifest
    ):
        result = await updater.check()

    assert result is not None
    assert result.version == "0.2.0"
    assert updater.pending_manifest is not None
    assert updater.state == UpdateState.IDLE


@pytest.mark.asyncio
async def test_check_no_update(updater):
    with patch.object(
        updater._checker, "check_for_update", new_callable=AsyncMock, return_value=None
    ):
        result = await updater.check()

    assert result is None
    assert updater.state == UpdateState.IDLE


@pytest.mark.asyncio
async def test_download_no_pending(updater):
    result = await updater.download_and_verify()
    assert result is False
    assert "No pending" in updater.error


@pytest.mark.asyncio
async def test_download_and_verify_success(updater, tmp_path):
    manifest = _make_manifest()
    updater._pending_manifest = manifest

    with patch.object(
        updater._downloader, "download", new_callable=AsyncMock, return_value=str(tmp_path / "f")
    ), patch(
        "ados.services.ota.updater.verify_sha256", return_value=True
    ):
        result = await updater.download_and_verify()

    assert result is True
    assert updater.state == UpdateState.IDLE


@pytest.mark.asyncio
async def test_download_and_verify_hash_fail(updater, tmp_path):
    manifest = _make_manifest()
    updater._pending_manifest = manifest

    with patch.object(
        updater._downloader, "download", new_callable=AsyncMock, return_value=str(tmp_path / "f")
    ), patch(
        "ados.services.ota.updater.verify_sha256", return_value=False
    ):
        result = await updater.download_and_verify()

    assert result is False
    assert updater.state == UpdateState.FAILED
    assert "SHA-256" in updater.error


@pytest.mark.asyncio
async def test_install_no_download(updater):
    result = await updater.install()
    assert result is False


@pytest.mark.asyncio
async def test_install_success(updater, tmp_path):
    manifest = _make_manifest()
    updater._pending_manifest = manifest

    # Create a fake downloaded file
    downloaded = tmp_path / "ados-0.2.0.bin"
    downloaded.write_bytes(b"update content")
    updater._downloaded_path = str(downloaded)

    with patch("ados.services.ota.updater.SLOT_A_PATH", str(tmp_path / "slot-a")), \
         patch("ados.services.ota.updater.SLOT_B_PATH", str(tmp_path / "slot-b")):
        # Create slot-a so config migration path exists
        (tmp_path / "slot-a").mkdir()
        result = await updater.install()

    assert result is True
    assert updater.state == UpdateState.IDLE


def test_get_status(updater):
    status = updater.get_status()
    assert status["state"] == "idle"
    assert status["current_version"] == "0.1.0"
    assert "download" in status
    assert "slots" in status
