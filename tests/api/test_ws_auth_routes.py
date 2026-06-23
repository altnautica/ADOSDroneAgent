"""Auth tests for the WebSocket routes routed through the unified
ticket-or-header helper.

Each route shares the same contract:

* ``X-ADOS-Key`` header on the handshake authenticates a native client.
* ``Sec-WebSocket-Protocol: ados-ws-ticket, <ticket>`` authenticates a
  browser client. The ticket is a self-contained HMAC token (minted by the
  native control surface, keyed off the pairing key) verified with no shared
  store, and is bound to a specific ``scope`` string.
* Anything else closes the handshake with code ``4401``.
* The previous ``?api_key=`` query-string fallback is gone.

Covers the setup cloudflare-logs socket. The ground-station PIC-events,
uplink-events, MAVLink-bridge, and mesh-events sockets are served natively by
the front (``ados-control``), which validates the same header-or-ticket auth in
its own handshake (covered by the ``ados-control`` crate tests). The ticket mint
itself is the native ``POST /api/_ws/ticket`` route; here the ticket is minted
directly via the Python verifier's mirror so the WS auth contract is exercised
end-to-end against the remaining Python route.
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from ados.api.middleware.ws_auth import WS_TICKET_PROTOCOL
from ados.api.server import create_app
from ados.core.ws_ticket import mint_ticket
from tests.api_runtime_utils import build_api_runtime

PAIR_KEY = "valid-pair-key"


def _make_paired_client(monkeypatch, profile: str = "ground_station") -> TestClient:
    app_double = build_api_runtime(uptime_seconds=0.0)
    app_double.pairing_manager.is_paired = True
    app_double.pairing_manager.api_key = PAIR_KEY
    app_double.pairing_manager.validate_key = lambda k: k == PAIR_KEY
    # Ground-station-gated routes need the right profile to reach the
    # ``accept()`` call after auth passes.
    app_double.config.agent.profile = profile
    # The ticket path derives its HMAC key from the pairing key the Rust mint
    # used (read from pairing.json in production); point it at the test key.
    monkeypatch.setattr(
        "ados.api.middleware.ws_auth.load_pairing_api_key",
        lambda *a, **k: PAIR_KEY,
    )
    return TestClient(create_app(app_double))


@pytest.fixture
def paired_drone_client(monkeypatch):
    """Cloudflare-logs route does not gate on profile, so the drone
    profile is fine for that endpoint's auth tests."""
    return _make_paired_client(monkeypatch, profile="drone")


def _expect_ws_rejected(client_inst: TestClient, url: str, **kwargs) -> None:
    with pytest.raises(WebSocketDisconnect) as excinfo:
        with client_inst.websocket_connect(url, **kwargs) as ws:
            ws.receive_json()
    assert excinfo.value.code == 4401


def _ticket(scope: str) -> str:
    return mint_ticket(scope, api_key=PAIR_KEY)


# ---------------------------------------------------------------------------
# /api/v1/setup/cloudflare/logs
# ---------------------------------------------------------------------------


def test_cloudflare_logs_ws_rejects_unauthenticated(paired_drone_client):
    _expect_ws_rejected(paired_drone_client, "/api/v1/setup/cloudflare/logs")


def test_cloudflare_logs_ws_rejects_bad_key(paired_drone_client):
    _expect_ws_rejected(
        paired_drone_client,
        "/api/v1/setup/cloudflare/logs",
        headers={"X-ADOS-Key": "wrong-key"},
    )


def test_cloudflare_logs_ws_rejects_api_key_query_param(paired_drone_client):
    """``?api_key=`` fallback is gone — the pairing key on the URL must NOT
    authenticate the handshake."""
    _expect_ws_rejected(
        paired_drone_client,
        "/api/v1/setup/cloudflare/logs?api_key=valid-pair-key",
    )


def test_cloudflare_logs_ws_accepts_header(paired_drone_client):
    with paired_drone_client.websocket_connect(
        "/api/v1/setup/cloudflare/logs",
        headers={"X-ADOS-Key": PAIR_KEY},
    ) as ws:
        assert ws is not None


def test_cloudflare_logs_ws_accepts_ticket(paired_drone_client):
    with paired_drone_client.websocket_connect(
        "/api/v1/setup/cloudflare/logs",
        subprotocols=[WS_TICKET_PROTOCOL, _ticket("setup.cloudflare_logs")],
    ) as ws:
        assert ws is not None


def test_cloudflare_logs_ws_rejects_wrong_scope_ticket(paired_drone_client):
    _expect_ws_rejected(
        paired_drone_client,
        "/api/v1/setup/cloudflare/logs",
        subprotocols=[WS_TICKET_PROTOCOL, _ticket("gs.mesh_events")],
    )


# Note: the PIC-events, uplink, mesh, and MAVLink WebSockets are no longer Python
# FastAPI routes — they are served natively by the front (`ados-control`), which
# validates the same header-or-ticket WebSocket auth in its own handshake
# (covered by the `ados-control` crate tests). Only the setup cloudflare-logs WS
# route above still flows through this Python helper.
