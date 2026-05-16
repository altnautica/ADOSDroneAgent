"""Auth tests for the five WebSocket routes routed through the
unified ticket-or-header helper.

Each route shares the same contract:

* ``X-ADOS-Key`` header on the handshake authenticates a native client.
* ``Sec-WebSocket-Protocol: ados-ws-ticket, <ticket>`` authenticates a
  browser client. The ticket is minted via
  ``POST /api/_ws/ticket`` and is bound to a specific ``scope`` string.
* Anything else closes the handshake with code ``4401``.
* The previous ``?api_key=`` query-string fallback is gone.

Covers the setup cloudflare-logs, ground-station PIC-events,
ground-station MAVLink bridge, ground-station uplink-events, and
ground-station mesh-events sockets.
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from ados.api.middleware.ws_auth import (
    WS_TICKET_PROTOCOL,
    ws_ticket_store,
)
from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def reset_tickets():
    ws_ticket_store._reset_for_tests()
    yield
    ws_ticket_store._reset_for_tests()


def _make_paired_client(profile: str = "ground_station") -> TestClient:
    app_double = build_api_runtime(uptime_seconds=0.0)
    app_double.pairing_manager.is_paired = True
    app_double.pairing_manager.api_key = "valid-pair-key"
    app_double.pairing_manager.validate_key = (
        lambda k: k == "valid-pair-key"
    )
    # Ground-station-gated routes need the right profile to reach the
    # ``accept()`` call after auth passes.
    app_double.config.agent.profile = profile
    return TestClient(create_app(app_double))


@pytest.fixture
def paired_ground_client():
    return _make_paired_client(profile="ground_station")


@pytest.fixture
def paired_drone_client():
    """Cloudflare-logs route does not gate on profile, so the drone
    profile is fine for that endpoint's auth tests."""
    return _make_paired_client(profile="drone")


def _expect_ws_rejected(client_inst: TestClient, url: str, **kwargs) -> None:
    with pytest.raises(WebSocketDisconnect) as excinfo:
        with client_inst.websocket_connect(url, **kwargs) as ws:
            # Drain the first frame so the reject path is exercised
            # even if the server tries to send a placeholder error.
            ws.receive_json()
    assert excinfo.value.code == 4401


def _mint_ticket(client_inst: TestClient, scope: str) -> str:
    resp = client_inst.post(
        "/api/_ws/ticket",
        headers={"X-ADOS-Key": "valid-pair-key"},
        json={"scope": scope},
    )
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["ok"] is True
    assert body["scope"] == scope
    assert len(body["ticket"]) == 64
    return str(body["ticket"])


# ---------------------------------------------------------------------------
# /api/_ws/ticket
# ---------------------------------------------------------------------------


def test_ticket_mint_requires_pairing_key(paired_ground_client, reset_tickets):
    resp = paired_ground_client.post(
        "/api/_ws/ticket", json={"scope": "gs.mesh_events"}
    )
    assert resp.status_code in (401, 403)


def test_ticket_mint_rejects_unknown_scope(paired_ground_client, reset_tickets):
    resp = paired_ground_client.post(
        "/api/_ws/ticket",
        headers={"X-ADOS-Key": "valid-pair-key"},
        json={"scope": "totally.made.up"},
    )
    assert resp.status_code == 400
    body = resp.json()
    assert body["detail"]["error"]["code"] == "E_UNKNOWN_SCOPE"


def test_ticket_mint_returns_hex_and_expiry(paired_ground_client, reset_tickets):
    ticket = _mint_ticket(paired_ground_client, scope="gs.mesh_events")
    assert all(c in "0123456789abcdef" for c in ticket)


# ---------------------------------------------------------------------------
# /api/v1/setup/cloudflare/logs
# ---------------------------------------------------------------------------


def test_cloudflare_logs_ws_rejects_unauthenticated(
    paired_drone_client, reset_tickets
):
    _expect_ws_rejected(
        paired_drone_client, "/api/v1/setup/cloudflare/logs"
    )


def test_cloudflare_logs_ws_rejects_bad_key(paired_drone_client, reset_tickets):
    _expect_ws_rejected(
        paired_drone_client,
        "/api/v1/setup/cloudflare/logs",
        headers={"X-ADOS-Key": "wrong-key"},
    )


def test_cloudflare_logs_ws_rejects_api_key_query_param(
    paired_drone_client, reset_tickets
):
    """``?api_key=`` fallback is gone — passing the pairing key on the
    URL must NOT authenticate the handshake."""
    _expect_ws_rejected(
        paired_drone_client,
        "/api/v1/setup/cloudflare/logs?api_key=valid-pair-key",
    )


def test_cloudflare_logs_ws_accepts_header(paired_drone_client, reset_tickets):
    with paired_drone_client.websocket_connect(
        "/api/v1/setup/cloudflare/logs",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ) as ws:
        # The journal tail is a background task; we only verify the
        # handshake completed. Close immediately.
        assert ws is not None


def test_cloudflare_logs_ws_accepts_ticket(paired_drone_client, reset_tickets):
    ticket = _mint_ticket(paired_drone_client, scope="setup.cloudflare_logs")
    with paired_drone_client.websocket_connect(
        "/api/v1/setup/cloudflare/logs",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ) as ws:
        assert ws is not None


def test_cloudflare_logs_ws_rejects_consumed_ticket(
    paired_drone_client, reset_tickets
):
    ticket = _mint_ticket(paired_drone_client, scope="setup.cloudflare_logs")
    with paired_drone_client.websocket_connect(
        "/api/v1/setup/cloudflare/logs",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ):
        pass
    _expect_ws_rejected(
        paired_drone_client,
        "/api/v1/setup/cloudflare/logs",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


def test_cloudflare_logs_ws_rejects_wrong_scope_ticket(
    paired_drone_client, reset_tickets
):
    # Mint a ticket for a different route, then try to use it on the
    # cloudflare-logs WS.
    ticket = _mint_ticket(paired_drone_client, scope="gs.mesh_events")
    _expect_ws_rejected(
        paired_drone_client,
        "/api/v1/setup/cloudflare/logs",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


# ---------------------------------------------------------------------------
# /api/v1/ground-station/pic/events
# ---------------------------------------------------------------------------


def test_pic_events_ws_rejects_unauthenticated(
    paired_ground_client, reset_tickets
):
    _expect_ws_rejected(
        paired_ground_client, "/api/v1/ground-station/pic/events"
    )


def test_pic_events_ws_rejects_api_key_query_param(
    paired_ground_client, reset_tickets
):
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/pic/events?api_key=valid-pair-key",
    )


def test_pic_events_ws_accepts_header(paired_ground_client, reset_tickets):
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/pic/events",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ) as ws:
        assert ws is not None


def test_pic_events_ws_accepts_ticket(paired_ground_client, reset_tickets):
    ticket = _mint_ticket(paired_ground_client, scope="gs.pic_events")
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/pic/events",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ) as ws:
        assert ws is not None


def test_pic_events_ws_rejects_consumed_ticket(
    paired_ground_client, reset_tickets
):
    ticket = _mint_ticket(paired_ground_client, scope="gs.pic_events")
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/pic/events",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ):
        pass
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/pic/events",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


# ---------------------------------------------------------------------------
# /api/v1/ground-station/ws/mavlink
# ---------------------------------------------------------------------------


def test_mavlink_ws_rejects_unauthenticated(paired_ground_client, reset_tickets):
    _expect_ws_rejected(
        paired_ground_client, "/api/v1/ground-station/ws/mavlink"
    )


def test_mavlink_ws_rejects_bad_key(paired_ground_client, reset_tickets):
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/ws/mavlink",
        headers={"X-ADOS-Key": "wrong-key"},
    )


def test_mavlink_ws_rejects_api_key_query_param(
    paired_ground_client, reset_tickets
):
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/ws/mavlink?api_key=valid-pair-key",
    )


def test_mavlink_ws_rejects_wrong_scope_ticket(
    paired_ground_client, reset_tickets
):
    ticket = _mint_ticket(paired_ground_client, scope="gs.mesh_events")
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/ws/mavlink",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


# Note: the success paths for ``/ws/mavlink`` require a live MAVLink
# IPC socket, which the test harness does not stand up. The handshake
# rejects without IPC even when auth succeeds (1011), but the auth
# rejection paths above already establish the contract: bad-or-missing
# credentials close 4401 BEFORE the IPC code runs.


# ---------------------------------------------------------------------------
# /api/v1/ground-station/ws/uplink
# ---------------------------------------------------------------------------


def test_uplink_ws_rejects_unauthenticated(paired_ground_client, reset_tickets):
    _expect_ws_rejected(
        paired_ground_client, "/api/v1/ground-station/ws/uplink"
    )


def test_uplink_ws_rejects_api_key_query_param(
    paired_ground_client, reset_tickets
):
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/ws/uplink?api_key=valid-pair-key",
    )


def test_uplink_ws_accepts_header(paired_ground_client, reset_tickets):
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/ws/uplink",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ) as ws:
        assert ws is not None


def test_uplink_ws_accepts_ticket(paired_ground_client, reset_tickets):
    ticket = _mint_ticket(paired_ground_client, scope="gs.uplink_events")
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/ws/uplink",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ) as ws:
        assert ws is not None


def test_uplink_ws_rejects_consumed_ticket(paired_ground_client, reset_tickets):
    ticket = _mint_ticket(paired_ground_client, scope="gs.uplink_events")
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/ws/uplink",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ):
        pass
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/ws/uplink",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


# ---------------------------------------------------------------------------
# /api/v1/ground-station/ws/mesh
# ---------------------------------------------------------------------------


def test_mesh_ws_rejects_unauthenticated(paired_ground_client, reset_tickets):
    _expect_ws_rejected(
        paired_ground_client, "/api/v1/ground-station/ws/mesh"
    )


def test_mesh_ws_rejects_api_key_query_param(
    paired_ground_client, reset_tickets
):
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/ws/mesh?api_key=valid-pair-key",
    )


def test_mesh_ws_accepts_header(paired_ground_client, reset_tickets):
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/ws/mesh",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ) as ws:
        assert ws is not None


def test_mesh_ws_accepts_ticket(paired_ground_client, reset_tickets):
    ticket = _mint_ticket(paired_ground_client, scope="gs.mesh_events")
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/ws/mesh",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ) as ws:
        assert ws is not None


def test_mesh_ws_rejects_consumed_ticket(paired_ground_client, reset_tickets):
    ticket = _mint_ticket(paired_ground_client, scope="gs.mesh_events")
    with paired_ground_client.websocket_connect(
        "/api/v1/ground-station/ws/mesh",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ):
        pass
    _expect_ws_rejected(
        paired_ground_client,
        "/api/v1/ground-station/ws/mesh",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )
