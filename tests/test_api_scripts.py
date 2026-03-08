"""Tests for the scripts API routes."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi.testclient import TestClient

from ados.api.routes.scripts import router


@pytest.fixture
def mock_app():
    """Mock AgentApp with demo scripting engine."""
    app = MagicMock()
    app._demo_scripting = MagicMock()
    app._demo_scripting.execute = AsyncMock(return_value="ok")
    app._demo_scripting.command_log = [
        {"timestamp": "2026-03-08T10:00:00", "command": "takeoff", "result": "ok"},
    ]
    app._demo_scripting.status.return_value = {
        "demo_mode": True,
        "sdk_mode": False,
        "altitude": 10.0,
        "armed": True,
        "mode": "LOITER",
        "battery": 90,
        "speed_cms": 100.0,
        "commands_executed": 1,
    }
    app._script_runner = None
    app._command_executor = None
    app._fc_connection = None
    return app


@pytest.fixture
def client(mock_app):
    """FastAPI test client with mocked agent app."""
    from fastapi import FastAPI

    test_app = FastAPI()
    test_app.include_router(router, prefix="/api")

    with patch("ados.api.routes.scripts.get_agent_app", return_value=mock_app):
        yield TestClient(test_app)


class TestScriptsApi:
    """API endpoint tests for scripting engine."""

    def test_list_scripts_empty(self, client):
        resp = client.get("/api/scripts")
        assert resp.status_code == 200
        data = resp.json()
        assert "scripts" in data
        assert "command_log" in data

    def test_scripting_status(self, client):
        resp = client.get("/api/scripting/status")
        assert resp.status_code == 200
        data = resp.json()
        assert data["demo_mode"] is True

    def test_execute_text_command(self, client, mock_app):
        resp = client.post("/api/scripting/command", json={"command": "takeoff"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "ok"
        mock_app._demo_scripting.execute.assert_called_once()

    def test_execute_command_error(self, client, mock_app):
        mock_app._demo_scripting.execute = AsyncMock(return_value="error: low battery")
        resp = client.post("/api/scripting/command", json={"command": "forward 100"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "error"
        assert "error" in data["result"]

    def test_run_script_no_runner(self, client):
        resp = client.post("/api/scripts/run", json={"path": "/tmp/test.py"})
        assert resp.status_code == 503

    def test_stop_script_no_runner(self, client):
        resp = client.post("/api/scripts/stop", json={"script_id": "abc123"})
        assert resp.status_code == 503


class TestScriptsApiWithRunner:
    """Tests with a mock script runner."""

    @pytest.fixture
    def runner_client(self, mock_app):
        from fastapi import FastAPI

        mock_runner = MagicMock()
        mock_runner.list_scripts.return_value = []
        mock_runner.start_script.return_value = "abc123456789"
        mock_runner.stop_script.return_value = True
        mock_app._script_runner = mock_runner

        test_app = FastAPI()
        test_app.include_router(router, prefix="/api")

        with patch("ados.api.routes.scripts.get_agent_app", return_value=mock_app):
            yield TestClient(test_app)

    def test_run_script(self, runner_client):
        resp = runner_client.post("/api/scripts/run", json={"path": "/tmp/test.py"})
        assert resp.status_code == 200
        assert resp.json()["script_id"] == "abc123456789"

    def test_stop_script(self, runner_client):
        resp = runner_client.post("/api/scripts/stop", json={"script_id": "abc123"})
        assert resp.status_code == 200

    def test_run_script_error(self, runner_client, mock_app):
        mock_app._script_runner.start_script.side_effect = RuntimeError("not found")
        resp = runner_client.post("/api/scripts/run", json={"path": "/bad.py"})
        assert resp.status_code == 400

    def test_stop_script_not_found(self, runner_client, mock_app):
        mock_app._script_runner.stop_script.return_value = False
        resp = runner_client.post("/api/scripts/stop", json={"script_id": "bad"})
        assert resp.status_code == 404
