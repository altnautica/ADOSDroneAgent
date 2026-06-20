"""Tests for the authoritative video-readiness probe.

The dashboard's video-state verdict must be decided by the :9997 paths-list
(`ready && source`), not by a flaky 1s WHEP GET whose only positive signal is a
405 ("bound", not "streaming"). The WHEP fallback fires only when :9997 is
unreachable / auth-blocked, and a 405 there is degraded, never ready.
"""

from __future__ import annotations

import httpx
import pytest

from ados.api.routes.video import _common


class _FakeResponse:
    def __init__(self, status_code: int, payload: object = None) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> object:
        if self._payload is None:
            raise ValueError("no json")
        return self._payload


class _FakeClient:
    """Stand-in for ``httpx.Client`` that answers the paths-list URL with a
    canned response and raises on anything else (so a test that hits the WHEP
    fallback URL must register it explicitly)."""

    def __init__(self, responses: dict[str, _FakeResponse]) -> None:
        self._responses = responses

    def __enter__(self) -> _FakeClient:
        return self

    def __exit__(self, *exc) -> None:
        return None

    def get(self, url: str) -> _FakeResponse:
        for needle, resp in self._responses.items():
            if needle in url:
                return resp
        raise httpx.ConnectError(f"no canned response for {url}")


@pytest.fixture
def patch_client(monkeypatch):
    def _install(responses: dict[str, _FakeResponse]) -> None:
        monkeypatch.setattr(
            _common.httpx, "Client", lambda *a, **k: _FakeClient(responses)
        )

    return _install


def test_ready_when_paths_list_reports_a_ready_source(patch_client) -> None:
    patch_client(
        {
            "/v3/paths/list": _FakeResponse(
                200,
                {
                    "items": [
                        {
                            "name": "main",
                            "ready": True,
                            "source": {"type": "rtmpConn"},
                            "tracks": ["H264"],
                            "bytesReceived": 123456,
                        }
                    ]
                },
            )
        }
    )
    ready, track = _common.mediamtx_ready_sync()
    assert ready is True
    assert track is not None and track.get("codec") == "H264"


def test_not_ready_when_paths_list_has_a_ready_path_with_no_source(patch_client) -> None:
    # A bound path that is "ready" but has no publisher source is NOT delivering.
    patch_client(
        {
            "/v3/paths/list": _FakeResponse(
                200,
                {"items": [{"name": "main", "ready": True, "source": None}]},
            )
        }
    )
    ready, _track = _common.mediamtx_ready_sync()
    assert ready is False


def test_not_ready_when_no_paths_yet(patch_client) -> None:
    patch_client({"/v3/paths/list": _FakeResponse(200, {"items": []})})
    ready, track = _common.mediamtx_ready_sync()
    assert ready is False
    assert track is None


def test_not_ready_when_paths_list_unreachable(patch_client) -> None:
    # :9997 raises (auth-blocked / down). The WHEP fallback proves only that the
    # endpoint is bound, never that frames flow → degraded, not ready.
    patch_client({})  # every URL raises ConnectError
    ready, track = _common.mediamtx_ready_sync()
    assert ready is False
    assert track is None


def test_whep_probe_405_is_bound_not_ready() -> None:
    # A 405 means the WHEP endpoint exists (bound), NOT that a publisher streams.
    # The async probe must report running:true but ready:false.
    import asyncio

    class _R:
        status_code = 405

    class _C:
        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc):
            return None

        async def get(self, _url):
            return _R()

    import ados.api.routes.video._common as common_mod

    orig = common_mod.httpx.AsyncClient
    common_mod.httpx.AsyncClient = lambda *a, **k: _C()
    try:
        result = asyncio.run(common_mod._probe_mediamtx_via_whep())
    finally:
        common_mod.httpx.AsyncClient = orig
    assert result is not None
    assert result["running"] is True
    assert result["ready"] is False, "a 405 is bound, not streaming"
