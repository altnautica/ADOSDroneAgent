"""Tests for the ground-station video-liveness heartbeat helper."""

from __future__ import annotations

from typing import Any

import ados.services.cloud.heartbeat as hb


class _Resp:
    def __init__(self, status_code: int, payload: Any) -> None:
        self.status_code = status_code
        self._payload = payload

    def json(self) -> Any:
        return self._payload


class _StubClient:
    """Sync httpx.Client stand-in: one canned /api/wfb response."""

    def __init__(self, resp: _Resp | Exception) -> None:
        self._resp = resp

    def __enter__(self) -> "_StubClient":
        return self

    def __exit__(self, *exc: object) -> bool:
        return False

    def get(self, url: str, *, headers: Any = None) -> _Resp:
        if isinstance(self._resp, Exception):
            raise self._resp
        return self._resp


def _patch_client(monkeypatch, resp: _Resp | Exception) -> None:
    import httpx

    monkeypatch.setattr(httpx, "Client", lambda *a, **k: _StubClient(resp))


def test_gs_video_live_when_connected_with_packets(monkeypatch) -> None:
    _patch_client(
        monkeypatch,
        _Resp(200, {"state": "connected", "packets_received": 1200}),
    )
    assert hb.read_gs_video_state(api_key="k") is True


def test_gs_video_not_live_when_no_packets(monkeypatch) -> None:
    _patch_client(
        monkeypatch,
        _Resp(200, {"state": "connected", "packets_received": 0}),
    )
    assert hb.read_gs_video_state(api_key="k") is False


def test_gs_video_not_live_when_stale(monkeypatch) -> None:
    _patch_client(
        monkeypatch,
        _Resp(200, {"state": "stale", "packets_received": 9000}),
    )
    assert hb.read_gs_video_state(api_key="k") is False


def test_gs_video_unknown_on_http_error(monkeypatch) -> None:
    _patch_client(monkeypatch, _Resp(401, {}))
    assert hb.read_gs_video_state(api_key=None) is None


def test_gs_video_unknown_on_exception(monkeypatch) -> None:
    _patch_client(monkeypatch, RuntimeError("boom"))
    assert hb.read_gs_video_state(api_key="k") is None
