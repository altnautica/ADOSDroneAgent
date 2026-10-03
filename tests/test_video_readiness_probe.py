"""Tests for the setup surface's primary-stream readiness probe.

The advertised ``/whep`` and HLS URLs address mediamtx's ``main`` path, so the
verdict is decided by that path alone (``ready`` AND a publisher ``source``),
never by whichever path the paths-list happens to name first.
"""

from __future__ import annotations

import asyncio
import importlib

import httpx
import pytest

# The package re-exports a function named `_access_urls` that shadows the
# submodule attribute, so import the module itself.
_access_urls = importlib.import_module("ados.setup.service._access_urls")


class _FakeResponse:
    def __init__(self, status_code: int, payload: object = None) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> object:
        if self._payload is None:
            raise ValueError("no json")
        return self._payload


class _FakeClient:
    """Stand-in for ``httpx.AsyncClient`` that answers the paths-list URL with
    a canned response, or raises when none is given."""

    def __init__(self, response: _FakeResponse | None) -> None:
        self._response = response

    async def __aenter__(self) -> _FakeClient:
        return self

    async def __aexit__(self, *exc) -> None:
        return None

    async def get(self, url: str) -> _FakeResponse:
        if self._response is None:
            raise httpx.ConnectError(f"no canned response for {url}")
        return self._response


@pytest.fixture
def paths_list(monkeypatch):
    def _install(response: _FakeResponse | None) -> None:
        monkeypatch.setattr(httpx, "AsyncClient", lambda *a, **k: _FakeClient(response))

    return _install


def _ready() -> bool:
    return asyncio.run(_access_urls._main_stream_ready())


def test_ready_when_main_has_a_publisher(paths_list) -> None:
    paths_list(
        _FakeResponse(
            200,
            {"items": [{"name": "main", "ready": True, "source": {"type": "rtspSession"}}]},
        )
    )
    assert _ready() is True


def test_a_ready_secondary_path_does_not_stand_in_for_an_absent_main(paths_list) -> None:
    paths_list(
        _FakeResponse(
            200,
            {"items": [{"name": "eo_wide", "ready": True, "source": {"type": "rtspSource"}}]},
        )
    )
    assert _ready() is False


def test_main_flagged_ready_without_a_publisher_is_not_ready(paths_list) -> None:
    paths_list(_FakeResponse(200, {"items": [{"name": "main", "ready": True, "source": None}]}))
    assert _ready() is False


def test_an_unreachable_or_credentialed_api_is_not_ready(paths_list) -> None:
    paths_list(None)
    assert _ready() is False
    paths_list(_FakeResponse(401, {"error": "authentication error"}))
    assert _ready() is False
