"""WebSocket auth for the in-flight install-job progress route.

The route accepts either the ``X-ADOS-Key`` header (native clients) or
an ``ados-ws-ticket`` minted for the job's ``plugins.install_job:<job_id>``
scope and passed through ``Sec-WebSocket-Protocol: ados-ws-ticket, <ticket>``,
the same self-contained HMAC ticket the control front admits for every
other stream. The pairing key never rides the URL.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from ados.api.middleware.ws_auth import WS_TICKET_PROTOCOL
from ados.api.routes import _plugins_helpers as helpers
from ados.api.routes import plugins as plugins_route
from ados.api.routes._plugins_helpers import job_stream_ticket_scope, write_sidecar
from ados.api.server import create_app
from ados.core.ws_ticket import mint_ticket
from ados.plugins.supervisor import PluginSupervisor
from tests.api_runtime_utils import build_api_runtime

PAIR_KEY = "valid-pair-key"


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
def paired_client(monkeypatch):
    app_double = build_api_runtime()
    app_double.pairing_manager.is_paired = True
    app_double.pairing_manager.api_key = PAIR_KEY
    app_double.pairing_manager.validate_key = lambda k: k == PAIR_KEY
    # The ticket key is read from pairing.json in production.
    monkeypatch.setattr(
        "ados.api.middleware.ws_auth.load_pairing_api_key",
        lambda *a, **k: PAIR_KEY,
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
# WebSocket subprotocol path
# ---------------------------------------------------------------------


def test_ws_accepts_an_install_job_ticket(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-ticket-ok"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    ticket = mint_ticket(job_stream_ticket_scope(job_id), api_key=PAIR_KEY)

    with paired_client.websocket_connect(
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    ) as ws:
        frame = ws.receive_json()
    assert frame["stage"] == "completed"
    assert frame["jobId"] == job_id


def test_ws_rejects_a_ticket_for_another_scope(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-wrong-scope"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    ticket = mint_ticket("vision.detections", api_key=PAIR_KEY)
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


def test_ws_rejects_a_ticket_for_another_job(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-mine"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    ticket = mint_ticket(job_stream_ticket_scope("job-other"), api_key=PAIR_KEY)
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


def test_ws_rejects_a_ticket_signed_by_another_key(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-forged"
    ticket = mint_ticket(job_stream_ticket_scope(job_id), api_key="some-other-key")
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}",
        subprotocols=[WS_TICKET_PROTOCOL, ticket],
    )


def test_the_retired_job_ticket_protocol_is_refused(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-old-marker"
    ticket = mint_ticket(job_stream_ticket_scope(job_id), api_key=PAIR_KEY)
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}",
        subprotocols=["ados-job-ticket", ticket],
    )
    # And the old per-job mint route is gone.
    resp = paired_client.post(
        f"/api/plugins/jobs/{job_id}/ticket",
        headers={"X-ADOS-Key": PAIR_KEY},
    )
    assert resp.status_code in (404, 405)


def test_ws_rejects_api_key_query_param(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
):
    """The old query-param fallback is gone; passing ``?api_key=``
    must no longer authenticate the handshake."""
    job_id = "job-no-qp"
    _expect_ws_rejected(
        paired_client,
        f"/api/plugins/jobs/{job_id}?api_key={PAIR_KEY}",
    )


def test_ws_header_still_accepts(
    paired_client,
    isolated_sidecar,
    isolated_supervisor,
    quick_ws,
):
    """Native clients keep using ``X-ADOS-Key`` on the handshake."""
    job_id = "job-hdr-keep"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    with paired_client.websocket_connect(
        f"/api/plugins/jobs/{job_id}",
        headers={"X-ADOS-Key": PAIR_KEY},
    ) as ws:
        frame = ws.receive_json()
    assert frame["stage"] == "completed"
