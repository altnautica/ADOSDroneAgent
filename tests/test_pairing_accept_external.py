"""Tests for the external-code accept path and the Convex SITE-origin
normalization.

The accept route (`claim_with_external_code`) is HTTP-200-on-failure; it must
return a truthful `{ok:false, error, message}` for every failure branch and, in
the default local posture, point the operator at the local-first fix (pair by
hostname/IP) rather than silently no-backending. The URL helper maps an
operator-entered backend coordinate to the HTTP-actions SITE origin where
`/pairing/register` lives.
"""

from __future__ import annotations

from ados.core.pairing import _normalize_convex_site_url, claim_with_external_code

# --- Convex SITE-origin normalization (the /pairing/register host) ---


def test_normalize_maps_selfhosted_backend_port() -> None:
    # Self-hosted: :3210 backend → :3211 HTTP-actions site.
    assert _normalize_convex_site_url("http://192.168.1.50:3210") == "http://192.168.1.50:3211"
    # A trailing slash is stripped.
    assert _normalize_convex_site_url("http://host:3210/") == "http://host:3211"


def test_normalize_maps_managed_backend_host() -> None:
    assert (
        _normalize_convex_site_url("https://convex.altnautica.com")
        == "https://convex-site.altnautica.com"
    )


def test_normalize_leaves_a_site_url_unchanged() -> None:
    assert (
        _normalize_convex_site_url("https://convex-site.altnautica.com")
        == "https://convex-site.altnautica.com"
    )
    assert _normalize_convex_site_url("http://host:3211") == "http://host:3211"
    assert _normalize_convex_site_url("") == ""
    assert _normalize_convex_site_url("   ") == ""


# --- claim_with_external_code: truthful failures ---


class _FakeServer:
    def __init__(self, mode: str, cloud_url: str = "", self_hosted_url: str = "") -> None:
        self.mode = mode
        self.cloud = type("C", (), {"url": cloud_url})()
        self.self_hosted = type("S", (), {"url": self_hosted_url})()


class _FakeConfig:
    def __init__(self, server: _FakeServer) -> None:
        self.server = server
        self.agent = type("A", (), {"device_id": "dev123456", "name": "t"})()


class _FakeApp:
    def __init__(self, config: _FakeConfig, paired: bool = False) -> None:
        self.config = config
        self.pairing_manager = type("P", (), {"is_paired": paired})()


async def test_accept_in_local_mode_fails_with_no_backend() -> None:
    app = _FakeApp(_FakeConfig(_FakeServer("local")))
    res = await claim_with_external_code(app, "ABC234")
    assert res["ok"] is False
    assert res["error"] == "no_backend"
    # The message points at the local-first fix (pair by hostname/IP), not cloud.
    assert "local" in res["message"].lower()


async def test_accept_with_short_code_is_rejected() -> None:
    app = _FakeApp(_FakeConfig(_FakeServer("local")))
    res = await claim_with_external_code(app, "AB")
    assert res["ok"] is False
    assert res["error"] == "invalid_code"


async def test_accept_when_already_paired_is_rejected() -> None:
    app = _FakeApp(_FakeConfig(_FakeServer("cloud", cloud_url="https://x")), paired=True)
    res = await claim_with_external_code(app, "ABC234")
    assert res["ok"] is False
    assert res["error"] == "already_paired"
