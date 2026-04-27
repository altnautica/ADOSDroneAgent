"""Tests for OTA update checker."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.core.config import OtaConfig
from ados.services.ota.checker import UpdateChecker, _version_tuple


def _make_release(
    version: str = "0.2.0",
    *,
    draft: bool = False,
    include_wheel: bool = True,
    include_sha: bool = True,
    body: str = "Fixes.",
) -> dict:
    """Build a GitHub Releases API JSON shape.

    The checker reads `tag_name`, walks `assets[]` for a wheel and
    optional SHA256SUMS asset, and pulls `body` + `html_url` +
    `published_at` for the manifest.
    """
    wheel_name = f"ados_drone_agent-{version}-py3-none-any.whl"
    assets: list[dict] = []
    if include_wheel:
        assets.append(
            {
                "name": wheel_name,
                "browser_download_url": (
                    f"https://github.com/altnautica/ADOSDroneAgent/releases/download/v{version}/{wheel_name}"
                ),
                "size": 1024,
            }
        )
    if include_sha:
        assets.append(
            {
                "name": "SHA256SUMS",
                "browser_download_url": (
                    f"https://github.com/altnautica/ADOSDroneAgent/releases/download/v{version}/SHA256SUMS"
                ),
                "size": 128,
            }
        )
    return {
        "tag_name": f"v{version}",
        "draft": draft,
        "prerelease": False,
        "published_at": "2026-03-08T00:00:00Z",
        "html_url": f"https://github.com/altnautica/ADOSDroneAgent/releases/tag/v{version}",
        "body": body,
        "assets": assets,
    }


def _make_sha_text(version: str = "0.2.0") -> str:
    """Build a SHA256SUMS text body matching what the wheel uses."""
    wheel_name = f"ados_drone_agent-{version}-py3-none-any.whl"
    digest = "a" * 64
    return f"{digest}  {wheel_name}\n"


def _patched_client(
    release_payload: dict,
    sha_text: str | None = None,
    release_status: int = 200,
):
    """Build the AsyncClient context manager mock the checker expects.

    The checker opens TWO httpx.AsyncClient contexts in sequence: one
    for the release JSON, one for the SHA256SUMS text. Each open spins
    up a fresh client. We use a module-scoped counter so each factory
    call returns the correct staged response.
    """
    release_resp = MagicMock()
    release_resp.status_code = release_status
    release_resp.json.return_value = release_payload
    release_resp.headers = {"ETag": "fake-etag"}
    release_resp.raise_for_status = MagicMock()

    sha_resp = MagicMock()
    sha_resp.status_code = 200
    sha_resp.text = sha_text or ""
    sha_resp.raise_for_status = MagicMock()

    responses = [release_resp, sha_resp]
    call_index = {"i": 0}

    def make_client(*_args, **_kwargs):
        client = AsyncMock()
        client.__aenter__ = AsyncMock(return_value=client)
        client.__aexit__ = AsyncMock(return_value=False)

        async def _get(*_a, **_kw):
            idx = call_index["i"]
            call_index["i"] = idx + 1
            return responses[idx] if idx < len(responses) else responses[-1]

        client.get = _get
        return client

    return make_client


def test_version_tuple_basic():
    assert _version_tuple("0.1.0") == (0, 1, 0)
    assert _version_tuple("1.2.3") == (1, 2, 3)
    assert _version_tuple("v2.0.0") == (2, 0, 0)


def test_version_tuple_comparison():
    assert _version_tuple("0.2.0") > _version_tuple("0.1.0")
    assert _version_tuple("1.0.0") > _version_tuple("0.99.99")
    assert _version_tuple("0.1.0") == _version_tuple("0.1.0")


@pytest.mark.asyncio
async def test_check_update_available():
    config = OtaConfig(server="https://test.example.com")
    checker = UpdateChecker(config)

    with patch(
        "ados.services.ota.checker.httpx.AsyncClient",
        side_effect=_patched_client(_make_release("0.2.0"), _make_sha_text("0.2.0")),
    ):
        result = await checker.check_for_update("0.1.0")

    assert result is not None
    assert result.version == "0.2.0"
    assert result.sha256 == "a" * 64
    assert checker.last_manifest is not None


@pytest.mark.asyncio
async def test_check_no_update_when_current():
    config = OtaConfig()
    checker = UpdateChecker(config)

    # Same version on the release as current -> no update.
    with patch(
        "ados.services.ota.checker.httpx.AsyncClient",
        side_effect=_patched_client(_make_release("0.1.0"), _make_sha_text("0.1.0")),
    ):
        result = await checker.check_for_update("0.1.0")

    assert result is None


@pytest.mark.asyncio
async def test_check_skips_old_version():
    config = OtaConfig()
    checker = UpdateChecker(config)

    # Release older than current -> no update.
    with patch(
        "ados.services.ota.checker.httpx.AsyncClient",
        side_effect=_patched_client(_make_release("0.0.5"), _make_sha_text("0.0.5")),
    ):
        result = await checker.check_for_update("0.1.0")

    assert result is None


@pytest.mark.asyncio
async def test_check_callback_fires():
    config = OtaConfig()
    found = []
    checker = UpdateChecker(config, on_update_found=lambda m: found.append(m))

    with patch(
        "ados.services.ota.checker.httpx.AsyncClient",
        side_effect=_patched_client(_make_release("0.2.0"), _make_sha_text("0.2.0")),
    ):
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


@pytest.mark.asyncio
async def test_check_handles_missing_wheel_asset():
    config = OtaConfig()
    checker = UpdateChecker(config)

    with patch(
        "ados.services.ota.checker.httpx.AsyncClient",
        side_effect=_patched_client(_make_release("0.2.0", include_wheel=False)),
    ):
        result = await checker.check_for_update("0.1.0")

    # No wheel asset means the release is unusable; checker returns None.
    assert result is None


@pytest.mark.asyncio
async def test_check_handles_rate_limit():
    config = OtaConfig()
    checker = UpdateChecker(config)

    rate_resp = MagicMock()
    rate_resp.status_code = 403
    rate_resp.headers = {}

    def factory(*_args, **_kwargs):
        client = AsyncMock()
        client.__aenter__ = AsyncMock(return_value=client)
        client.__aexit__ = AsyncMock(return_value=False)
        client.get = AsyncMock(return_value=rate_resp)
        return client

    with patch("ados.services.ota.checker.httpx.AsyncClient", side_effect=factory):
        result = await checker.check_for_update("0.1.0")

    assert result is None
