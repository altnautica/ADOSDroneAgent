"""Wi-Fi client REST surface — profile-agnostic mount.

Smoke-tests for the new ``/api/v1/network/client/*`` router that
exposes scan / status / configured / join / leave / forget /
autoconnect to any profile. The handlers all delegate to the
singleton ``WifiClientManager`` so the test patches the singleton
factory and asserts the routes return whatever the manager produced.
"""

from __future__ import annotations

from unittest.mock import AsyncMock, patch

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client() -> TestClient:
    runtime = build_api_runtime(uptime_seconds=0.0)
    return TestClient(create_app(runtime))


@pytest.fixture
def fake_manager():
    manager = AsyncMock()
    with patch(
        "ados.services.ground_station.wifi_client_manager.get_wifi_client_manager",
        return_value=manager,
    ):
        yield manager


class TestNetworkWifi:
    def test_scan_returns_networks_list(self, client, fake_manager):
        fake_manager.scan.return_value = [
            {"ssid": "A", "bssid": "x", "signal": 90, "security": "WPA2", "in_use": False},
            {"ssid": "B", "bssid": "y", "signal": 60, "security": "--", "in_use": False},
        ]
        resp = client.get("/api/v1/network/client/scan")
        assert resp.status_code == 200
        body = resp.json()
        assert len(body["networks"]) == 2
        assert body["networks"][0]["ssid"] == "A"

    def test_autoconnect_enable(self, client, fake_manager):
        fake_manager.set_autoconnect.return_value = {
            "autoconnect": True,
            "name": "HomeWifi",
            "error": None,
        }
        resp = client.put(
            "/api/v1/network/client/configured/HomeWifi/autoconnect",
            json={"enabled": True},
        )
        assert resp.status_code == 200
        assert resp.json()["autoconnect"] is True

    def test_autoconnect_disable(self, client, fake_manager):
        fake_manager.set_autoconnect.return_value = {
            "autoconnect": False,
            "name": "HomeWifi",
            "error": None,
        }
        resp = client.put(
            "/api/v1/network/client/configured/HomeWifi/autoconnect",
            json={"enabled": False},
        )
        assert resp.status_code == 200
        assert resp.json()["autoconnect"] is False

    # The drone path: no uplink daemon, so the native front forwards status and
    # the join/leave/forget writes here. These used to have no handler at all.

    def test_status_reports_the_manager_state(self, client, fake_manager):
        fake_manager.status.return_value = {
            "connected": True,
            "ssid": "BenchNet",
            "bssid": None,
            "signal": 70,
            "ip": "192.168.1.50",
            "gateway": "192.168.1.1",
            "security": "WPA2",
        }
        resp = client.get("/api/v1/network/client/status")
        assert resp.status_code == 200
        assert resp.json()["connected"] is True

    def test_status_is_unknown_when_the_manager_fails(self, client, fake_manager):
        fake_manager.status.side_effect = RuntimeError("nmcli missing")
        resp = client.get("/api/v1/network/client/status")
        assert resp.status_code == 503
        assert resp.json()["detail"]["error"]["code"] == "E_WIFI_STATUS_UNAVAILABLE"

    def test_join_returns_the_join_body(self, client, fake_manager):
        fake_manager.join.return_value = {
            "joined": True,
            "ip": "192.168.1.50",
            "gateway": "192.168.1.1",
            "error": None,
        }
        resp = client.put(
            "/api/v1/network/client/join", json={"ssid": "BenchNet", "passphrase": "pw"}
        )
        assert resp.status_code == 200
        assert resp.json() == {
            "joined": True,
            "ip": "192.168.1.50",
            "gateway": "192.168.1.1",
            "error": None,
        }

    def test_join_with_an_active_ap_needs_force(self, client, fake_manager):
        fake_manager.join.return_value = {
            "joined": False,
            "error": "station_busy_ap_active",
            "hint": "Stop AP first or force",
        }
        resp = client.put("/api/v1/network/client/join", json={"ssid": "BenchNet"})
        assert resp.status_code == 409
        assert resp.json()["needs_force"] is True

    def test_leave_and_forget(self, client, fake_manager):
        fake_manager.leave.return_value = {"left": True, "previous_ssid": "BenchNet"}
        resp = client.delete("/api/v1/network/client")
        assert resp.status_code == 200
        assert resp.json()["left"] is True

        fake_manager.forget.return_value = {"forgot": True, "name": "BenchNet", "error": None}
        assert client.delete("/api/v1/network/client/configured/BenchNet").status_code == 200

        fake_manager.forget.return_value = {"forgot": False, "name": "Nope", "error": "not_found"}
        resp = client.delete("/api/v1/network/client/configured/Nope")
        assert resp.status_code == 400
        assert resp.json()["detail"]["error"]["message"] == "not_found"
