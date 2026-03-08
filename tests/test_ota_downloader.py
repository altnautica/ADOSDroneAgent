"""Tests for OTA update downloader."""

from __future__ import annotations

import os
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.ota.downloader import (
    DownloadProgress,
    DownloadState,
    UpdateDownloader,
)
from ados.services.ota.manifest import UpdateManifest


def _make_manifest(**overrides) -> UpdateManifest:
    data = {
        "version": "0.2.0",
        "channel": "stable",
        "release_date": "2026-03-08T00:00:00Z",
        "download_url": "https://updates.altnautica.com/stable/ados-0.2.0.bin",
        "file_size": 256,
        "sha256": "a" * 64,
        "signature": "c2ln",
        "min_version": "0.1.0",
        "changelog": "Fixes.",
        "requires_reboot": False,
    }
    data.update(overrides)
    return UpdateManifest(**data)


def test_download_progress_percent():
    p = DownloadProgress(bytes_downloaded=50, total_bytes=100)
    assert p.percent() == 50.0


def test_download_progress_percent_zero():
    p = DownloadProgress(bytes_downloaded=0, total_bytes=0)
    assert p.percent() == 0.0


def test_download_progress_default_state():
    p = DownloadProgress()
    assert p.state == DownloadState.IDLE
    assert p.bytes_downloaded == 0


@pytest.mark.asyncio
async def test_download_success(tmp_path):
    manifest = _make_manifest(file_size=12)
    downloader = UpdateDownloader()

    progress_updates = []
    downloader.add_progress_callback(lambda p: progress_updates.append(p.state))

    # Mock httpx streaming
    async def mock_aiter_bytes(chunk_size=65536):
        yield b"hello world!"

    mock_response = AsyncMock()
    mock_response.raise_for_status = lambda: None
    mock_response.aiter_bytes = mock_aiter_bytes

    mock_client = AsyncMock()
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)
    mock_client.stream = MagicMock(return_value=AsyncMock(
        __aenter__=AsyncMock(return_value=mock_response),
        __aexit__=AsyncMock(return_value=False),
    ))

    with patch("ados.services.ota.downloader.httpx.AsyncClient", return_value=mock_client):
        filepath = await downloader.download(manifest, str(tmp_path))

    assert os.path.exists(filepath)
    assert filepath.endswith("ados-0.2.0.bin")
    assert downloader.progress.state == DownloadState.COMPLETED


@pytest.mark.asyncio
async def test_download_creates_target_dir(tmp_path):
    target = tmp_path / "subdir" / "nested"
    manifest = _make_manifest(file_size=5)
    downloader = UpdateDownloader()

    async def mock_aiter_bytes(chunk_size=65536):
        yield b"12345"

    mock_response = AsyncMock()
    mock_response.raise_for_status = lambda: None
    mock_response.aiter_bytes = mock_aiter_bytes

    mock_client = AsyncMock()
    mock_client.__aenter__ = AsyncMock(return_value=mock_client)
    mock_client.__aexit__ = AsyncMock(return_value=False)
    mock_client.stream = MagicMock(return_value=AsyncMock(
        __aenter__=AsyncMock(return_value=mock_response),
        __aexit__=AsyncMock(return_value=False),
    ))

    with patch("ados.services.ota.downloader.httpx.AsyncClient", return_value=mock_client):
        filepath = await downloader.download(manifest, str(target))

    assert os.path.exists(filepath)
    assert target.exists()
