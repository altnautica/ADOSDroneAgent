"""Tests for the plugin install-job WebSocket progress stream.

The route is transport-agnostic: both the LAN-direct install and the
cloud-relay receiver write the same sidecar JSON at
``/run/ados/plugin_install_<jobId>.json``. The WebSocket route polls
mtime on that file and streams each change to the GCS until a terminal
stage (completed / failed / cancelled) or idle timeout.
"""

from __future__ import annotations

import time
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from ados.api.routes import _plugins_helpers as helpers
from ados.api.routes import plugins as plugins_route
from ados.api.routes._plugins_helpers import write_sidecar
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
    """Speed up the poll so tests don't drag."""
    monkeypatch.setattr(helpers, "WS_POLL_INTERVAL_SECONDS", 0.01, raising=False)


@pytest.fixture
def client(monkeypatch):
    app_double = build_api_runtime(uptime_seconds=0.0)
    # The capability-token route reads pairing_manager.api_key; not
    # exercised here but keep the wiring complete. Default fixture
    # keeps the agent UNPAIRED so the streaming tests below match the
    # open-on-unpaired posture of the HTTP middleware.
    app_double.pairing_manager.api_key = "test-pair-key"
    app_double.pairing_manager.is_paired = False
    return TestClient(create_app(app_double))


@pytest.fixture
def paired_client(monkeypatch):
    """Variant of ``client`` that simulates a paired agent.

    The auth-on-WebSocket tests use this so the helper exercises the
    full validation chain (header path, query-param fallback,
    rejection path).
    """
    app_double = build_api_runtime(uptime_seconds=0.0)
    app_double.pairing_manager.is_paired = True
    app_double.pairing_manager.api_key = "valid-pair-key"
    app_double.pairing_manager.validate_key = (
        lambda k: k == "valid-pair-key"
    )
    # security.api.api_key path: leave unset so the test runs the
    # pairing-key validation branch.
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


# ---------------------------------------------------------------------
# Stream + close
# ---------------------------------------------------------------------


def test_ws_streams_stage_updates_until_completed(
    client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-ws-1"
    # Seed the sidecar before connecting so the first poll has data.
    write_sidecar(job_id, {"stage": "verifying", "pluginId": "p.x"})

    with client.websocket_connect(f"/api/plugins/jobs/{job_id}") as ws:
        first = ws.receive_json()
        assert first["stage"] == "verifying"
        assert first["pluginId"] == "p.x"
        assert first["jobId"] == job_id

        # Mutate the sidecar — small sleep so mtime resolution catches.
        time.sleep(0.05)
        write_sidecar(job_id, {"stage": "installing", "pluginId": "p.x"})
        second = ws.receive_json()
        assert second["stage"] == "installing"

        # Terminal stage closes the stream after one final frame.
        time.sleep(0.05)
        write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
        terminal = ws.receive_json()
        assert terminal["stage"] == "completed"

    # After exiting the context manager the server-side coroutine has
    # already returned (no exception means clean close).


def test_ws_closes_on_failed_stage(
    client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-ws-2"
    write_sidecar(job_id, {"stage": "failed", "pluginId": "p.x", "detail": "boom"})
    with client.websocket_connect(f"/api/plugins/jobs/{job_id}") as ws:
        frame = ws.receive_json()
        assert frame["stage"] == "failed"
        assert frame["detail"] == "boom"
    # Exit-without-exception confirms the server closed after the
    # terminal frame.


def test_ws_idle_timeout_cancels(
    client, isolated_sidecar, isolated_supervisor, monkeypatch
):
    monkeypatch.setattr(helpers, "WS_POLL_INTERVAL_SECONDS", 0.005, raising=False)
    monkeypatch.setattr(helpers, "WS_IDLE_TIMEOUT_SECONDS", 0.05, raising=False)
    job_id = "job-ws-idle"
    # No sidecar seed; the route should idle-out and send a cancelled
    # frame before closing.
    with client.websocket_connect(f"/api/plugins/jobs/{job_id}") as ws:
        frame = ws.receive_json()
        assert frame["stage"] == "cancelled"
        assert frame["reason"] == "idle_timeout"


def test_ws_no_sidecar_then_appears(
    client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-ws-late"
    # Connect first, then create the sidecar mid-stream.
    with client.websocket_connect(f"/api/plugins/jobs/{job_id}") as ws:
        # Sidecar absent for the first few polls; route should keep waiting.
        time.sleep(0.05)
        write_sidecar(job_id, {"stage": "queued", "pluginId": "late.plug"})
        frame = ws.receive_json()
        assert frame["stage"] == "queued"

        time.sleep(0.05)
        write_sidecar(job_id, {"stage": "completed", "pluginId": "late.plug"})
        terminal = ws.receive_json()
        assert terminal["stage"] == "completed"


# ---------------------------------------------------------------------
# Capability-token mint endpoint (HTTP, not WS, but lives in same router)
# ---------------------------------------------------------------------


def test_capability_token_mint_returns_signed_token(
    client, isolated_sidecar, isolated_supervisor
):
    # Verifier needs a paired manager. The fixture seeded api_key="test-pair-key".
    # No installed plugin yet, so this should 404 first.
    resp = client.post(
        "/api/plugins/capability-token", json={"plugin_id": "com.absent"}
    )
    assert resp.status_code == 404


def test_capability_token_mint_not_paired(
    isolated_sidecar, isolated_supervisor, monkeypatch
):
    app_double = build_api_runtime(uptime_seconds=0.0)
    app_double.pairing_manager.api_key = None
    test_client = TestClient(create_app(app_double))
    resp = test_client.post(
        "/api/plugins/capability-token", json={"plugin_id": "com.example"}
    )
    assert resp.status_code == 409
    body = resp.json()
    assert body["kind"] == "not_paired"


# ---------------------------------------------------------------------
# WebSocket auth (BaseHTTPMiddleware bypass closed)
# ---------------------------------------------------------------------
#
# ``ApiKeyAuthMiddleware`` extends Starlette's HTTP middleware base and
# never processes the WebSocket handshake. The route enforces the
# paired-key contract inline. Three cases covered here:
#   (a) no key                 → close 4401
#   (b) bad / unknown key      → close 4401
#   (c) ``X-ADOS-Key`` header  → accepts + streams (native clients)
#
# The browser-only one-shot ticket subprotocol path is exercised in
# ``tests/test_plugins_ws_ticket_auth.py``. The previous
# ``?api_key=...`` query-string fallback was retired with that change.


def _expect_ws_rejected(client_inst, url: str) -> None:
    """Helper: connecting must fail with a WebSocket close before accept."""
    from starlette.websockets import WebSocketDisconnect as StarletteWSDisconnect

    with pytest.raises(StarletteWSDisconnect) as excinfo:
        with client_inst.websocket_connect(url) as ws:
            ws.receive_json()
    # 4401 is the agent's app-defined close code for "auth required".
    assert excinfo.value.code == 4401


def test_ws_paired_no_key_rejected(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    _expect_ws_rejected(paired_client, "/api/plugins/jobs/job-noauth")


def test_ws_paired_query_string_no_longer_accepted(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    """The legacy ``?api_key=...`` fallback was removed so the
    pairing key cannot leak into request URLs. Even a perfectly
    valid pairing key passed this way must be rejected."""
    _expect_ws_rejected(
        paired_client, "/api/plugins/jobs/job-old-qp?api_key=valid-pair-key"
    )


def test_ws_paired_valid_header_accepts(
    paired_client, isolated_sidecar, isolated_supervisor, quick_ws
):
    job_id = "job-hdr-ok"
    write_sidecar(job_id, {"stage": "completed", "pluginId": "p.x"})
    with paired_client.websocket_connect(
        f"/api/plugins/jobs/{job_id}",
        headers={"X-ADOS-Key": "valid-pair-key"},
    ) as ws:
        frame = ws.receive_json()
    assert frame["stage"] == "completed"
    assert frame["jobId"] == job_id
