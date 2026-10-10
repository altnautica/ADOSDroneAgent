"""Setup-wizard mutations answer only ``{"ok": true}``."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.setup import state as setup_state
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client(monkeypatch):
    calls: list[str] = []
    monkeypatch.setattr(setup_state, "mark_finalized", lambda: calls.append("finalized"))
    monkeypatch.setattr(setup_state, "mark_setup_skipped", lambda: calls.append("skipped"))
    monkeypatch.setattr(setup_state, "reset_state", lambda: calls.append("reset"))
    test_client = TestClient(create_app(build_api_runtime()))
    test_client.calls = calls  # type: ignore[attr-defined]
    return test_client


def test_finish_returns_only_ok(client) -> None:
    resp = client.post("/api/v1/setup/finish")
    assert resp.status_code == 200
    assert resp.json() == {"ok": True}
    assert client.calls == ["finalized"]


@pytest.mark.parametrize(("path", "call"), [("skip", "skipped"), ("reset", "reset")])
def test_skip_and_reset_return_only_ok(client, path: str, call: str) -> None:
    resp = client.post(f"/api/v1/setup/{path}")
    assert resp.status_code == 200
    assert resp.json() == {"ok": True}
    assert client.calls == [call]
