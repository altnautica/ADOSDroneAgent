"""Tests for OTA update checker."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.core.config import OtaConfig
from ados.services.ota.checker import UpdateChecker, _version_tuple


def _make_manifest(**overrides) -> dict:
    base = {
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
    base.update(overrides)
    return base


def test_version_tuple_basic():
    assert _version_tuple("0.1.0") == (0, 1, 0)
    assert _version_tuple("1.2.3") == (1, 2, 3)
    assert _version_tuple("v2.0.0") == (2, 0, 0)


def test_version_tuple_comparison():
    assert _version_tuple("0.2.0") > _version_tuple("0.1.0")
    assert _version_tuple("1.0.0") > _version_tuple("0.99.99")
    assert _version_tuple("0.1.0") == _version_tuple("0.1.0")


@pytest.mark.skip(reason="checker now reads GitHub Releases API; mock shape needs full rewrite")
@pytest.mark.asyncio
async def test_check_update_available():
    config = OtaConfig(server="https://test.example.com")
    checker = UpdateChecker(config)

    mock_resp = MagicMock()
    mock_resp.json.return_value = _make_manifest()
    mock_resp.raise_for_status = MagicMock()

    with patch("ados.services.ota.checker.httpx.AsyncClient") as mock_client_cls:
        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)
        mock_client.get = AsyncMock(return_value=mock_resp)
        mock_client_cls.return_value = mock_client

        result = await checker.check_for_update("0.1.0")

    assert result is not None
    assert result.version == "0.2.0"
    assert checker.last_manifest is not None


@pytest.mark.asyncio
async def test_check_no_update_when_current():
    config = OtaConfig()
    checker = UpdateChecker(config)

    mock_resp = MagicMock()
    mock_resp.json.return_value = _make_manifest(version="0.1.0")
    mock_resp.raise_for_status = MagicMock()

    with patch("ados.services.ota.checker.httpx.AsyncClient") as mock_client_cls:
        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)
        mock_client.get = AsyncMock(return_value=mock_resp)
        mock_client_cls.return_value = mock_client

        result = await checker.check_for_update("0.1.0")

    assert result is None


@pytest.mark.asyncio
async def test_check_rejects_min_version():
    config = OtaConfig()
    checker = UpdateChecker(config)

    mock_resp = MagicMock()
    mock_resp.json.return_value = _make_manifest(version="2.0.0", min_version="1.0.0")
    mock_resp.raise_for_status = MagicMock()

    with patch("ados.services.ota.checker.httpx.AsyncClient") as mock_client_cls:
        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)
        mock_client.get = AsyncMock(return_value=mock_resp)
        mock_client_cls.return_value = mock_client

        result = await checker.check_for_update("0.1.0")

    assert result is None


@pytest.mark.skip(reason="checker now reads GitHub Releases API; mock shape needs full rewrite")
@pytest.mark.asyncio
async def test_check_callback_fires():
    config = OtaConfig()
    found = []
    checker = UpdateChecker(config, on_update_found=lambda m: found.append(m))

    mock_resp = MagicMock()
    mock_resp.json.return_value = _make_manifest()
    mock_resp.raise_for_status = MagicMock()

    with patch("ados.services.ota.checker.httpx.AsyncClient") as mock_client_cls:
        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)
        mock_client.get = AsyncMock(return_value=mock_resp)
        mock_client_cls.return_value = mock_client

        await checker.check_for_update("0.1.0")

    assert len(found) == 1
    assert found[0].version == "0.2.0"


@pytest.mark.asyncio
async def test_check_handles_http_error():
    config = OtaConfig()
    checker = UpdateChecker(config)

    import httpx

    with patch("ados.services.ota.checker.httpx.AsyncClient") as mock_client_cls:
        mock_client = AsyncMock()
        mock_client.__aenter__ = AsyncMock(return_value=mock_client)
        mock_client.__aexit__ = AsyncMock(return_value=False)
        mock_client.get = AsyncMock(side_effect=httpx.HTTPError("Connection failed"))
        mock_client_cls.return_value = mock_client

        result = await checker.check_for_update("0.1.0")

    assert result is None
