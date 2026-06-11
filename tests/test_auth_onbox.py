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
