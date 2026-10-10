"""WebSocket handshake auth on the residual surface."""

from __future__ import annotations

import asyncio
from types import SimpleNamespace

import pytest

from ados.api.middleware import ws_auth


class _Socket:
    def __init__(self, headers: dict[str, str]) -> None:
        self.headers = headers
        self.closed: tuple[int, str] | None = None

    async def close(self, code: int, reason: str) -> None:
        self.closed = (code, reason)


class _Pairing:
    def __init__(self, paired: bool, key: str = "") -> None:
        self.is_paired = paired
        self._key = key

    def validate_key(self, key: str) -> bool:
        return bool(self._key) and key == self._key


def _app(monkeypatch, pm, configured_key: str = "") -> None:
    app = SimpleNamespace(
        pairing_manager=pm,
        config=SimpleNamespace(
            security=SimpleNamespace(api=SimpleNamespace(api_key=configured_key))
        ),
    )
    monkeypatch.setattr("ados.api.deps.get_agent_app", lambda: app)
    monkeypatch.setattr(ws_auth, "load_pairing_api_key", lambda: None)


def _run(sock: _Socket) -> str | None:
    return asyncio.run(ws_auth.authenticate_websocket(sock, scope="vision.detections"))


def test_the_onbox_stamp_admits_a_paired_node(monkeypatch) -> None:
    _app(monkeypatch, _Pairing(True, "k"))
    assert _run(_Socket({"x-ados-onbox": "1"})) == ""


def test_an_unpaired_node_refuses_a_bare_handshake(monkeypatch) -> None:
    _app(monkeypatch, _Pairing(False))
    sock = _Socket({})
    assert _run(sock) is None
    assert sock.closed == (4401, "auth required")


def test_an_unpaired_node_ignores_an_arbitrary_key(monkeypatch) -> None:
    _app(monkeypatch, _Pairing(False))
    assert _run(_Socket({"X-ADOS-Key": "anything"})) is None


def test_keys_are_compared_with_compare_digest(monkeypatch) -> None:
    calls: list[tuple[str, str]] = []
    real = ws_auth.hmac.compare_digest

    def spy(a, b):
        calls.append((a, b))
        return real(a, b)

    monkeypatch.setattr(ws_auth.hmac, "compare_digest", spy)
    _app(monkeypatch, _Pairing(True, "pair"), configured_key="cfg-key")
    assert _run(_Socket({"X-ADOS-Key": "cfg-key"})) == ""
    assert calls == [("cfg-key", "cfg-key")]
    assert _run(_Socket({"X-ADOS-Key": "pair"})) == ""
    assert _run(_Socket({"X-ADOS-Key": "wrong"})) is None


@pytest.mark.parametrize("value", ["0", "true", ""])
def test_only_the_exact_onbox_value_is_trusted(monkeypatch, value: str) -> None:
    _app(monkeypatch, _Pairing(True, "k"))
    assert _run(_Socket({"x-ados-onbox": value})) is None
