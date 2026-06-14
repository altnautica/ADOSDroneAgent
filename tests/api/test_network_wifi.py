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
    def test_status_returns_manager_dict(self, client, fake_manager):
        fake_manager.status.return_value = {
            "connected": True,
            "ssid": "BenchWifi",
            "bssid": "aa:bb:cc:dd:ee:ff",
            "signal": 72,
            "ip": "192.168.1.42",
            "gateway": "192.168.1.1",
            "security": "WPA2",
        }
        resp = client.get("/api/v1/network/client/status")
        assert resp.status_code == 200
        assert resp.json()["ssid"] == "BenchWifi"
        assert resp.json()["connected"] is True

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

    def test_configured_returns_connections_list(self, client, fake_manager):
        fake_manager.configured_connections.return_value = [
            {"name": "HomeWifi", "type": "802-11-wireless", "device": "wlan0", "autoconnect": True},
        ]
        resp = client.get("/api/v1/network/client/configured")
        assert resp.status_code == 200
        body = resp.json()
        assert len(body["connections"]) == 1
        assert body["connections"][0]["name"] == "HomeWifi"

    def test_autoconnect_enable(self, client, fake_manager):
        fake_manager.set_autoconnect.return_value = {
            "autoconnect": True, "name": "HomeWifi", "error": None,
        }
        resp = client.put(
            "/api/v1/network/client/configured/HomeWifi/autoconnect",
            json={"enabled": True},
        )
        assert resp.status_code == 200
        assert resp.json()["autoconnect"] is True

    def test_autoconnect_disable(self, client, fake_manager):
        fake_manager.set_autoconnect.return_value = {
            "autoconnect": False, "name": "HomeWifi", "error": None,
        }
        resp = client.put(
            "/api/v1/network/client/configured/HomeWifi/autoconnect",
            json={"enabled": False},
        )
        assert resp.status_code == 200
        assert resp.json()["autoconnect"] is False
