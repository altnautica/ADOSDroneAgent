"""Tests for WFB-ng API routes."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.services.wfb.demo import DemoWfbManager
from ados.services.wfb.link_quality import LinkStats
from ados.services.wfb.manager import LinkState
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def agent_app():
    """Create an API runtime double with no WFB manager."""
    return build_api_runtime(wfb_manager=None)


@pytest.fixture
def client(agent_app):
    """FastAPI test client."""
    fastapi_app = create_app(agent_app)
    return TestClient(fastapi_app)


def test_wfb_status_no_manager(client):
    """GET /api/wfb returns disabled state when no WFB manager."""
    resp = client.get("/api/wfb")
    assert resp.status_code == 200
    data = resp.json()
    assert data["state"] == "disabled"
    assert data["rssi_dbm"] == -100.0


def test_wfb_status_with_demo(agent_app):
    """GET /api/wfb returns demo data when demo manager is set."""
    demo = DemoWfbManager()
    demo._state = LinkState.CONNECTED
    stats = LinkStats(rssi_dbm=-55.0, packets_received=1000, loss_percent=0.5)
    demo._monitor._latest = stats
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.get("/api/wfb")
    assert resp.status_code == 200
    data = resp.json()
    assert data["state"] == "connected"
    assert data["rssi_dbm"] == -55.0


def test_wfb_history_no_manager(client):
    """GET /api/wfb/history returns empty when no manager."""
    resp = client.get("/api/wfb/history")
    assert resp.status_code == 200
    data = resp.json()
    assert data["samples"] == []
    assert data["count"] == 0


def test_wfb_history_with_data(agent_app):
    """GET /api/wfb/history returns samples from monitor."""
    demo = DemoWfbManager()
    # Feed some stats
    for i in range(5):
        line = (
            f"rssi_min=-{50+i} rssi_avg=-{48+i} rssi_max=-{46+i} "
            f"packets={1000+i} lost={i} fec_rec=0 fec_fail=0"
        )
        demo.monitor.feed_line(line)
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.get("/api/wfb/history?seconds=60")
    assert resp.status_code == 200
    data = resp.json()
    assert data["count"] == 5
    assert len(data["samples"]) == 5
    assert "rssi_dbm" in data["samples"][0]


def test_wfb_set_channel_valid(agent_app):
    """POST /api/wfb/channel with valid channel."""
    demo = DemoWfbManager()
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.post("/api/wfb/channel", json={"channel": 36})
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "ok"
    assert data["channel"] == 36
    assert data["frequency_mhz"] == 5180


def test_wfb_set_channel_invalid(agent_app):
    """POST /api/wfb/channel with invalid channel returns 400."""
    demo = DemoWfbManager()
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.post("/api/wfb/channel", json={"channel": 999})
    assert resp.status_code == 400


def test_wfb_set_channel_no_manager(client):
    """POST /api/wfb/channel without manager returns 503."""
    resp = client.post("/api/wfb/channel", json={"channel": 149})
    assert resp.status_code == 503


def test_wfb_status_includes_tx_power_fields(agent_app):
    """GET /api/wfb surfaces tx_power_dbm/max/topology/mcs_index."""
    demo = DemoWfbManager()
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.get("/api/wfb")
    assert resp.status_code == 200
    data = resp.json()
    assert "tx_power_dbm" in data
    assert "tx_power_max_dbm" in data
    assert "topology" in data
    assert "mcs_index" in data
    assert "regulatory_domain" in data


def test_wfb_status_falls_back_to_config_when_no_manager(agent_app):
    """GET /api/wfb without a manager still returns config-derived defaults."""
    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)
    resp = client.get("/api/wfb")
    assert resp.status_code == 200
    data = resp.json()
    # tx_power_max_dbm sourced from config defaults (15 dBm).
    assert data["tx_power_max_dbm"] == 15
    assert data["topology"] == "host_vbus"


def test_wfb_set_tx_power_below_floor(agent_app):
    """PUT /api/wfb/tx-power with 0 returns 400 below_floor."""
    demo = DemoWfbManager()
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.put("/api/wfb/tx-power", json={"tx_power_dbm": 0})
    assert resp.status_code == 400
    detail = resp.json()["detail"]
    assert detail["error"] == "below_floor"
    assert detail["min"] == 1


def test_wfb_set_tx_power_above_ceiling(agent_app):
    """PUT /api/wfb/tx-power with 99 returns 400 above_ceiling with the configured max."""
    demo = DemoWfbManager()
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.put("/api/wfb/tx-power", json={"tx_power_dbm": 99})
    assert resp.status_code == 400
    detail = resp.json()["detail"]
    assert detail["error"] == "above_ceiling"
    # Default ceiling on a fresh ADOSConfig is 15 dBm.
    assert detail["max"] == 15


def test_wfb_set_tx_power_accepts_in_range(agent_app, tmp_path, monkeypatch):
    """PUT /api/wfb/tx-power within range applies via the manager and returns 200."""
    # Redirect persistence away from /etc/ados during tests.
    fake_config = tmp_path / "config.yaml"
    monkeypatch.setattr(
        "ados.api.routes.wfb.CONFIG_YAML",
        fake_config,
    )

    demo = DemoWfbManager()
    agent_app.wfb_manager_handle = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.put("/api/wfb/tx-power", json={"tx_power_dbm": 7})
    assert resp.status_code == 200
    body = resp.json()
    assert body["requested_dbm"] == 7
    # Demo manager echoes back the clamped value.
    assert body["effective_dbm"] == 7
    assert body["tx_power_max_dbm"] == 15
    # Persisted on disk.
    assert fake_config.exists()


def test_wfb_set_tx_power_no_manager(client):
    """PUT /api/wfb/tx-power without a manager returns 503."""
    resp = client.put("/api/wfb/tx-power", json={"tx_power_dbm": 5})
    assert resp.status_code == 503


def test_wfb_set_tx_power_apply_failure(agent_app):
    """PUT /api/wfb/tx-power surfaces a 500 when the driver call raises."""
    class BrokenDemo(DemoWfbManager):
        def apply_tx_power(self, dbm: int):
            raise RuntimeError("driver rejected")

    agent_app.wfb_manager_handle = BrokenDemo()

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.put("/api/wfb/tx-power", json={"tx_power_dbm": 5})
    assert resp.status_code == 500
    detail = resp.json()["detail"]
    assert detail["error"] == "apply_failed"


# ---------------------------------------------------------------------
# Pair failover-status sidecar
# ---------------------------------------------------------------------


def test_failover_status_missing_sidecar_defaults_to_local(
    client, tmp_path, monkeypatch,
):
    """No sidecar file => failover_state defaults to ``local``."""
    monkeypatch.setattr(
        "ados.api.routes.wfb.FAILOVER_STATE_PATH",
        tmp_path / "missing.json",
    )

    resp = client.get("/api/wfb/pair/failover-status")
    assert resp.status_code == 200
    assert resp.json() == {"failover_state": "local"}


def test_failover_status_local(client, tmp_path, monkeypatch):
    """Sidecar with ``local`` state echoes through unchanged."""
    sidecar = tmp_path / "wfb_failover.json"
    sidecar.write_text('{"state": "local"}', encoding="utf-8")
    monkeypatch.setattr("ados.api.routes.wfb.FAILOVER_STATE_PATH", sidecar)

    resp = client.get("/api/wfb/pair/failover-status")
    assert resp.status_code == 200
    assert resp.json() == {"failover_state": "local"}


def test_failover_status_cloud_relay(client, tmp_path, monkeypatch):
    """Sidecar with ``cloud_relay`` state echoes through unchanged."""
    sidecar = tmp_path / "wfb_failover.json"
    sidecar.write_text('{"state": "cloud_relay"}', encoding="utf-8")
    monkeypatch.setattr("ados.api.routes.wfb.FAILOVER_STATE_PATH", sidecar)

    resp = client.get("/api/wfb/pair/failover-status")
    assert resp.status_code == 200
    assert resp.json() == {"failover_state": "cloud_relay"}


def test_failover_status_failed(client, tmp_path, monkeypatch):
    """Sidecar with ``failed`` state echoes through unchanged."""
    sidecar = tmp_path / "wfb_failover.json"
    sidecar.write_text('{"state": "failed"}', encoding="utf-8")
    monkeypatch.setattr("ados.api.routes.wfb.FAILOVER_STATE_PATH", sidecar)

    resp = client.get("/api/wfb/pair/failover-status")
    assert resp.status_code == 200
    assert resp.json() == {"failover_state": "failed"}


def test_failover_status_unknown_value_falls_back_to_local(
    client, tmp_path, monkeypatch,
):
    """An unrecognized state defends with ``local`` rather than echoing."""
    sidecar = tmp_path / "wfb_failover.json"
    sidecar.write_text('{"state": "weird"}', encoding="utf-8")
    monkeypatch.setattr("ados.api.routes.wfb.FAILOVER_STATE_PATH", sidecar)

    resp = client.get("/api/wfb/pair/failover-status")
    assert resp.status_code == 200
    assert resp.json() == {"failover_state": "local"}


def test_failover_status_corrupt_json(client, tmp_path, monkeypatch):
    """A non-JSON file returns ``local`` instead of bubbling up an error."""
    sidecar = tmp_path / "wfb_failover.json"
    sidecar.write_text("not json {{{", encoding="utf-8")
    monkeypatch.setattr("ados.api.routes.wfb.FAILOVER_STATE_PATH", sidecar)

    resp = client.get("/api/wfb/pair/failover-status")
    assert resp.status_code == 200
    assert resp.json() == {"failover_state": "local"}
