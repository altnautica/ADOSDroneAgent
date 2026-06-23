"""WebSocket ticket flow for the in-flight install-job progress route.

Previously the route accepted ``?api_key=<pairing_key>`` in the URL
query string so browsers could authenticate the handshake. That leaks
the pairing key into DevTools, HAR exports, and reverse-proxy access
logs. The route now requires either the ``X-ADOS-Key`` header (native
clients) or a one-shot ticket passed through the
``Sec-WebSocket-Protocol: ados-job-ticket, <ticket>`` subprotocol
header.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from ados.api.routes import _plugins_helpers as helpers
from ados.api.routes import plugins as plugins_route
from ados.api.routes._plugins_helpers import (
    WS_JOB_TICKET_PROTOCOL,
    job_ticket_store,
    write_sidecar,
)
from ados.api.server import create_app
from ados.plugins.supervisor import PluginSupervisor
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def isolated_sidecar(tmp_path: Path, monkeypatch):
    sidecar_root = tmp_path / "run"
    sidecar_root.mkdir()
    monkeypatch.setattr(helpers, "SIDECAR_DIR", sidecar_root, raising=False)
    return sidecar_root


@pytest.fixture
def quick_ws(monkeypatch):
    monkeypatch.setattr(helpers, "WS_POLL_INTERVAL_SECONDS", 0.01, raising=False)


@pytest.fixture
def reset_ticket_store():
    """Each test starts with a clean ticket dict so cross-test state
    cannot mask issues."""
    job_ticket_store._reset_for_tests()
    yield
    job_ticket_store._reset_for_tests()


@pytest.fixture
def paired_client(monkeypatch):
    app_double = build_api_runtime(uptime_seconds=0.0)
    app_double.pairing_manager.is_paired = True
    app_double.pairing_manager.api_key = "valid-pair-key"
    app_double.pairing_manager.validate_key = (
        lambda k: k == "valid-pair-key"
    )
    return TestClient(create_app(app_double))


@pytest.fixture
def isolated_supervisor(tmp_path: Path, monkeypatch):
    state_path = tmp_path / "state.json"
    monkeypatch.setattr(
        "ados.plugins.state.PLUGIN_STATE_PATH", state_path, raising=False
    )
    sup = PluginSupervisor(
        install_dir=tmp_path / "var-plugins", require_signed=False
    )
    sup.discover()
    plugins_route._set_supervisor_for_tests(sup)
    yield sup
    plugins_route._set_supervisor_for_tests(None)


def _expect_ws_rejected(client_inst, url: str, **kwargs) -> None:
    from starlette.websockets import WebSocketDisconnect as StarletteWSDisconnect

    with pytest.raises(StarletteWSDisconnect) as excinfo:
        with client_inst.websocket_connect(url, **kwargs) as ws:
            ws.receive_json()
    assert excinfo.value.code == 4401


# ---------------------------------------------------------------------
# Ticket mint endpoint
# ---------------------------------------------------------------------


def test_ticket_mint_returns_hex_and_expiry(
    paired_client, isolated_sidecar, isolated_supervisor, reset_ticket_store
):
    job_id = "job-mint-1"
    resp = paired_client.post(
        f"/api/plugins/jobs/{job_id}/ticket",
        headers={"X-ADOS-Key": "valid-pair-key"},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    assert isinstance(body["ticket"], str)
    assert len(body["ticket"]) == 64
    assert all(c in "0123456789abcdef" for c in body["ticket"])
    assert isinstance(body["expiresAt"], int)
    assert body["expiresAt"] > 0


# ---------------------------------------------------------------------
# WebSocket subprotocol path
# ---------------------------------------------------------------------


def test_ws_accepts_with_valid_ticket_subprotocol(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
    reset_ticket_store,
):
    job_id = "job-ticket-ok"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    mint = paired_client.post(
        f"/api/plugins/jobs/{job_id}/ticket",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ).json()
    ticket = mint["ticket"]

    with paired_client.websocket_connect(
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_JOB_TICKET_PROTOCOL, ticket],
    ) as ws:
        frame = ws.receive_json()
    assert frame["stage"] == "completed"
    assert frame["jobId"] == job_id


def test_ws_ticket_is_one_shot(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
    reset_ticket_store,
):
    job_id = "job-ticket-once"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    ticket = paired_client.post(
        f"/api/plugins/jobs/{job_id}/ticket",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ).json()["ticket"]

    with paired_client.websocket_connect(
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_JOB_TICKET_PROTOCOL, ticket],
    ) as ws:
        ws.receive_json()

    # Same ticket again — must be rejected.
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_JOB_TICKET_PROTOCOL, ticket],
    )


def test_ws_ticket_must_match_job_id(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
    reset_ticket_store,
):
    issuing_job = "job-A"
    target_job = "job-B"
    write_sidecar(target_job, {"stage": "completed", "pluginId": "p.x"})
    ticket = paired_client.post(
        f"/api/plugins/jobs/{issuing_job}/ticket",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ).json()["ticket"]

    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{target_job}",
        subprotocols=[WS_JOB_TICKET_PROTOCOL, ticket],
    )


def test_ws_rejects_unknown_ticket(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
    reset_ticket_store,
):
    job_id = "job-unknown-ticket"
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_JOB_TICKET_PROTOCOL, "f" * 64],
    )


def test_ws_rejects_api_key_query_param(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
    reset_ticket_store,
):
    """The old query-param fallback is gone; passing ``?api_key=``
    must no longer authenticate the handshake."""
    job_id = "job-no-qp"
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}?api_key=valid-pair-key",
    )


def test_ws_header_still_accepts(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
    reset_ticket_store,
):
    """Native clients keep using ``X-ADOS-Key`` on the handshake."""
    job_id = "job-hdr-keep"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    with paired_client.websocket_connect(
        f"/api/plugins/jobs/{job_id}",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ) as ws:
        frame = ws.receive_json()
    assert frame["stage"] == "completed"
