"""Tests for the Convex SITE-origin normalization.

The cloud-choice writer stores the pairing backend URL through this helper, which
maps an operator-entered backend coordinate to the HTTP-actions SITE origin where
`/pairing/register` lives.
"""

from __future__ import annotations

from ados.core.pairing import _normalize_convex_site_url


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
