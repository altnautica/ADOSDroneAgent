"""Paired-agent auth: same-origin trust is scoped to setup mutations only.

An attacker controls the Origin/Referer/Host headers, so a blanket same-origin
bypass on general paired routes would let a forged header reach an authenticated
endpoint without the API key. These tests pin that a spoofed-origin request to a
paired non-setup route is rejected without a key, that a keyed request passes,
and that the narrowly-scoped setup-mutation surface still admits a same-origin
browser without a key.
"""

from __future__ import annotations

from typing import Any

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime

_KNOWN_KEY = "test-pairing-key-abc123"
# The host an attacker spoofs across Origin / Referer / Host so the legacy
# blanket bypass would have treated the request as same-origin.
_SPOOF_HOST = "attacker.example"


def _paired_client(profile: str = "auto") -> TestClient:
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    runtime = build_api_runtime(config=cfg)
    runtime.pairing_manager.is_paired = True
    runtime.pairing_manager.validate_key = lambda key: key == _KNOWN_KEY
    return TestClient(create_app(runtime))


def _spoofed_headers(extra: dict[str, str] | None = None) -> dict[str, str]:
    headers = {
        "Host": _SPOOF_HOST,
        "Origin": f"http://{_SPOOF_HOST}",
        "Referer": f"http://{_SPOOF_HOST}/dashboard",
    }
    if extra:
        headers.update(extra)
    return headers


def test_spoofed_origin_without_key_is_rejected() -> None:
    """A paired non-setup route rejects a spoofed-origin request lacking the key."""
    client = _paired_client()
    resp = client.post(
        "/api/command", json={"cmd": "arm"}, headers=_spoofed_headers()
    )
    assert resp.status_code == 401
    assert "X-ADOS-Key" in resp.json()["detail"]


def test_spoofed_origin_with_key_passes_auth() -> None:
    """The same spoofed-origin request WITH the key clears the auth layer.

    The FC is disconnected in the test double, so the command route then 503s;
    the point is the request is NOT a 401 — auth admitted it because the key is
    valid, not because of any same-origin shortcut.
    """
    client = _paired_client()
    resp = client.post(
        "/api/command",
        json={"cmd": "arm"},
        headers=_spoofed_headers({"X-ADOS-Key": _KNOWN_KEY}),
    )
    assert resp.status_code != 401
    assert resp.status_code == 503  # FC not connected — past auth


def test_get_config_spoofed_origin_without_key_is_rejected() -> None:
    """A paired GET route is also rejected on a spoofed origin without a key."""
    client = _paired_client()
    resp = client.get("/api/config", headers=_spoofed_headers())
    assert resp.status_code == 401


def test_setup_mutation_same_origin_still_passes_without_key(monkeypatch) -> None:
    """A SAME_ORIGIN_SETUP_PATHS route still admits a same-origin browser.

    The setup surface keeps the physical-presence-on-the-LAN gate: a same-origin
    POST without an API key is admitted by auth and the route runs. The route's
    persist side effect is stubbed so the test isolates the auth boundary.
    """
    from ados.setup import state as setup_state

    monkeypatch.setattr(setup_state, "mark_setup_skipped", lambda: None)

    client = _paired_client()
    same_origin_host = "testserver"
    resp = client.post(
        "/api/v1/setup/skip",
        headers={
            "Host": same_origin_host,
            "Origin": f"http://{same_origin_host}",
            "Referer": f"http://{same_origin_host}/setup",
        },
    )
    assert resp.status_code == 200, resp.text


def test_setup_mutation_spoofed_origin_with_token_required_rejected() -> None:
    """With setup_token_required, a same-origin setup POST without the token 401s.

    Confirms the setup surface still honours the token escalation knob and does
    not silently fall through to the general paired path.
    """
    cfg = ADOSConfig()
    cfg.security.setup_token_required = True
    runtime: Any = build_api_runtime(config=cfg)
    runtime.pairing_manager.is_paired = True
    runtime.pairing_manager.validate_key = lambda key: key == _KNOWN_KEY
    client = TestClient(create_app(runtime))

    resp = client.post(
        "/api/v1/setup/skip",
        headers={
            "Host": "testserver",
            "Origin": "http://testserver",
        },
    )
    assert resp.status_code == 401
    assert "Setup-Token" in resp.json()["detail"]


@pytest.mark.parametrize("path", ["/api/command", "/api/config"])
def test_no_origin_header_without_key_is_rejected(path: str) -> None:
    """A server-to-server caller (no Origin) on a paired route needs the key."""
    client = _paired_client()
    method = client.post if path == "/api/command" else client.get
    kwargs = {"json": {"cmd": "arm"}} if path == "/api/command" else {}
    resp = method(path, headers={"Host": "testserver"}, **kwargs)
    assert resp.status_code == 401
