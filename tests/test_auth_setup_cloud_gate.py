"""Auth boundary for the cloud-posture-mutating setup routes + the paired 401 hint.

The setup same-origin allowlist used to admit a forged-Origin LAN caller (no
key) to impactful routes: ``cloud-choice`` flips the agent's cloud posture and
``remote-access/cloudflare`` writes a root-owned tunnel token. These tests pin
that the cloud-posture routes now require a real credential (API key or setup
token) regardless of paired state, that the cosmetic wizard routes still admit a
same-origin browser without a key, that the dead ``navigation/*`` allowlist
entries are gone, and that the paired-route 401 body points the operator at
``ados status`` for the key.
"""

from __future__ import annotations

from typing import Any

import pytest
from fastapi.testclient import TestClient

from ados.api import middleware
from ados.api.middleware import auth as auth_mod
from ados.api.middleware.auth import (
    SAME_ORIGIN_SETUP_CLOUD_PATHS,
    SAME_ORIGIN_SETUP_PATHS,
)
from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime

_KNOWN_KEY = "test-pairing-key-abc123"
_SPOOF_HOST = "attacker.example"
_SETUP_TOKEN = "setup-token-xyz789"

_CLOUD_CHOICE = "/api/v1/setup/cloud-choice"


def _spoofed_headers(extra: dict[str, str] | None = None) -> dict[str, str]:
    headers = {
        "Host": _SPOOF_HOST,
        "Origin": f"http://{_SPOOF_HOST}",
        "Referer": f"http://{_SPOOF_HOST}/setup",
    }
    if extra:
        headers.update(extra)
    return headers


def _client(*, paired: bool = False, token_required: bool = False) -> TestClient:
    cfg = ADOSConfig()
    if token_required:
        cfg.security.setup_token_required = True
    runtime: Any = build_api_runtime(config=cfg)
    runtime.pairing_manager.is_paired = paired
    runtime.pairing_manager.validate_key = lambda key: key == _KNOWN_KEY
    return TestClient(create_app(runtime))


def test_cloud_routes_split_out_of_cosmetic_allowlist() -> None:
    """The cloud-posture routes are their own set and are NOT in the cosmetic
    same-origin allowlist (so the bare same-origin pass can never reach them)."""
    assert _CLOUD_CHOICE in SAME_ORIGIN_SETUP_CLOUD_PATHS
    assert "/api/v1/setup/remote-access/cloudflare" in SAME_ORIGIN_SETUP_CLOUD_PATHS
    assert SAME_ORIGIN_SETUP_PATHS.isdisjoint(SAME_ORIGIN_SETUP_CLOUD_PATHS)
    # The cosmetic set is wizard progress only.
    assert SAME_ORIGIN_SETUP_PATHS == {
        "/api/v1/setup/finish",
        "/api/v1/setup/skip",
        "/api/v1/setup/reset",
    }


def test_dead_navigation_entries_removed() -> None:
    """The non-existent setup/navigation/* routes are gone from both sets."""
    combined = SAME_ORIGIN_SETUP_PATHS | SAME_ORIGIN_SETUP_CLOUD_PATHS
    assert not any("navigation" in p for p in combined)


def test_cloud_choice_forged_origin_no_key_unpaired_is_rejected() -> None:
    """A forged-Origin no-key POST to cloud-choice on a FRESH (unpaired) agent
    is rejected — it can no longer flip cloud posture via same-origin."""
    client = _client(paired=False)
    resp = client.post(
        _CLOUD_CHOICE, json={"mode": "local"}, headers=_spoofed_headers()
    )
    assert resp.status_code == 401
    assert "cloud posture" in resp.json()["detail"]


def test_cloud_choice_forged_origin_no_key_paired_is_rejected() -> None:
    """Same rejection on a paired agent."""
    client = _client(paired=True)
    resp = client.post(
        _CLOUD_CHOICE, json={"mode": "local"}, headers=_spoofed_headers()
    )
    assert resp.status_code == 401
    assert "cloud posture" in resp.json()["detail"]


def test_cloud_choice_with_key_clears_auth() -> None:
    """With the API key the cloud route clears auth (not a 401)."""
    client = _client(paired=True)
    resp = client.post(
        _CLOUD_CHOICE,
        json={"mode": "local"},
        headers=_spoofed_headers({"X-ADOS-Key": _KNOWN_KEY}),
    )
    assert resp.status_code != 401


def test_cloud_choice_with_setup_token_clears_auth(monkeypatch) -> None:
    """A valid setup token also clears the cloud-route auth gate."""
    monkeypatch.setattr(auth_mod, "_load_setup_token", lambda: _SETUP_TOKEN)
    client = _client(paired=False)
    resp = client.post(
        _CLOUD_CHOICE,
        json={"mode": "local"},
        headers=_spoofed_headers({"X-ADOS-Setup-Token": _SETUP_TOKEN}),
    )
    assert resp.status_code != 401


def test_cosmetic_setup_skip_still_passes_same_origin(monkeypatch) -> None:
    """A cosmetic wizard route still admits a same-origin browser without a key."""
    from ados.setup import state as setup_state

    monkeypatch.setattr(setup_state, "mark_setup_skipped", lambda: None)

    client = _client(paired=True)
    host = "testserver"
    resp = client.post(
        "/api/v1/setup/skip",
        headers={"Host": host, "Origin": f"http://{host}", "Referer": f"http://{host}/setup"},
    )
    assert resp.status_code == 200, resp.text


def test_paired_route_401_points_operator_at_ados_status() -> None:
    """A direct-visit (no key) paired route 401 names `ados status` so the
    operator can find the key instead of seeing a blank/hung dashboard."""
    client = _client(paired=True)
    resp = client.get("/api/config", headers={"Host": "testserver"})
    assert resp.status_code == 401
    assert "ados status" in resp.json()["detail"]


def test_middleware_namespace_reexports_cloud_set() -> None:
    """Smoke: the middleware package still imports cleanly with the new set."""
    assert hasattr(middleware, "auth")
    assert isinstance(SAME_ORIGIN_SETUP_CLOUD_PATHS, set)


@pytest.mark.parametrize("path", list(SAME_ORIGIN_SETUP_CLOUD_PATHS))
def test_all_cloud_routes_reject_forged_origin_no_key(path: str) -> None:
    """Every cloud-posture route rejects a forged-Origin no-key request."""
    client = _client(paired=False)
    resp = client.post(path, json={}, headers=_spoofed_headers())
    assert resp.status_code == 401
