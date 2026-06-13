"""On-box loopback trust for the API auth middleware.

A paired agent requires X-ADOS-Key on authenticated routes, but the on-box CLI
cannot read the 0600-root pairing key, so it relies on loopback trust. These
tests pin that contract (loopback peer + no proxy-forwarding header) so the
behaviour stays stable across the future native control-surface port.

The request is duck-typed (only ``.client.host`` and case-insensitive
``in`` membership on ``.headers`` are read) so the test needs no web framework.
"""

from __future__ import annotations

from types import SimpleNamespace

from ados.api.middleware.auth import _is_on_box


class _Headers:
    """Minimal case-insensitive header container matching the subset of the
    Starlette ``Headers`` contract that ``_is_on_box`` relies on."""

    def __init__(self, data: dict[str, str]):
        self._keys = {k.lower() for k in data}

    def __contains__(self, key: str) -> bool:
        return key.lower() in self._keys


def _req(client, headers: dict[str, str] | None = None):
    host = SimpleNamespace(host=client[0]) if client else None
    return SimpleNamespace(client=host, headers=_Headers(headers or {}))


def test_loopback_ipv4_is_on_box():
    assert _is_on_box(_req(("127.0.0.1", 50321))) is True


def test_loopback_ipv6_is_on_box():
    assert _is_on_box(_req(("::1", 50321))) is True


def test_lan_peer_is_not_on_box():
    assert _is_on_box(_req(("192.168.1.50", 50321))) is False


def test_missing_client_is_not_on_box():
    assert _is_on_box(_req(None)) is False


def test_proxied_loopback_is_not_on_box():
    # A reverse proxy / tunnel terminating on 127.0.0.1 carries a forwarding
    # header — it must not be trusted as an on-box caller.
    for header in ("X-Forwarded-For", "CF-Connecting-IP", "Forwarded", "X-Real-IP"):
        assert _is_on_box(_req(("127.0.0.1", 50321), {header: "203.0.113.7"})) is False


class _GetHeaders(_Headers):
    """Header container that also supports ``.get`` (case-insensitive), used by
    the behind-front path that honours the trustworthy ``X-ADOS-Onbox`` header."""

    def __init__(self, data: dict[str, str]):
        super().__init__(data)
        self._map = {k.lower(): v for k, v in data.items()}

    def get(self, key: str, default=None):
        return self._map.get(key.lower(), default)


def _front_req(client, headers: dict[str, str] | None = None):
    host = SimpleNamespace(host=client[0]) if client else None
    return SimpleNamespace(client=host, headers=_GetHeaders(headers or {}))


def test_behind_front_honours_the_trustworthy_onbox_header(monkeypatch):
    # When served behind the native front (internal socket), request.client is
    # the socket (unreliable), so on-box trust comes from the X-ADOS-Onbox the
    # front stamps. The front already validated loopback, so even a non-loopback
    # client.host is trusted when the header says 1.
    monkeypatch.setenv("ADOS_API_INTERNAL_SOCKET", "/run/ados/api-internal.sock")
    assert _is_on_box(_front_req(("192.168.1.50", 1), {"X-ADOS-Onbox": "1"})) is True
    # Absent or non-"1" header behind the front means not on-box.
    assert _is_on_box(_front_req(("127.0.0.1", 1), {})) is False
    assert _is_on_box(_front_req(("127.0.0.1", 1), {"X-ADOS-Onbox": "0"})) is False


def test_onbox_header_is_ignored_when_not_behind_the_front(monkeypatch):
    # Without the internal-socket env, FastAPI owns the TCP port directly, so an
    # off-box client could spoof the header — it must NOT grant trust. Only the
    # real loopback peer does.
    monkeypatch.delenv("ADOS_API_INTERNAL_SOCKET", raising=False)
    assert _is_on_box(_front_req(("192.168.1.50", 1), {"X-ADOS-Onbox": "1"})) is False
    assert _is_on_box(_front_req(("127.0.0.1", 1), {"X-ADOS-Onbox": "1"})) is True
