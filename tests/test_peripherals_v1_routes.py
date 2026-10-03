"""The peripheral REST surface refuses what nothing on the agent implements.

A config write has no consumer and a declared action may have no dispatcher;
both must be refused rather than answered with a success the operator would
trust, and the listing must not offer an action that cannot run.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from ados.api.routes import peripherals_v1
from ados.services.peripherals.registry import PeripheralRegistry

_MANIFEST = """\
id: ados.rtl8812eu-radio
display_name: Radio
transport: usb
actions:
  - id: restart_radio
    display_name: Restart radio
  - id: rescan
    display_name: Rescan
config_schema:
  type: object
"""


@pytest.fixture
def client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> TestClient:
    (tmp_path / "radio.yaml").write_text(_MANIFEST)
    registry = PeripheralRegistry(glob_path=str(tmp_path / "*.yaml"))
    monkeypatch.setattr(peripherals_v1, "get_peripheral_registry", lambda: registry)
    app = FastAPI()
    app.include_router(peripherals_v1.router, prefix="/api")
    return TestClient(app)


def test_config_write_is_refused_not_reported_persisted(client: TestClient) -> None:
    resp = client.post("/api/v1/peripherals/ados.rtl8812eu-radio/config", json={"a": 1})
    assert resp.status_code == 501
    assert resp.json()["detail"]["error"]["code"] == "E_NOT_SUPPORTED"


def test_config_write_for_an_unknown_peripheral_is_not_found(client: TestClient) -> None:
    resp = client.post("/api/v1/peripherals/nope/config", json={})
    assert resp.status_code == 404


def test_declared_action_without_a_dispatcher_is_refused(client: TestClient) -> None:
    resp = client.post(
        "/api/v1/peripherals/ados.rtl8812eu-radio/action",
        json={"action_id": "rescan"},
    )
    assert resp.status_code == 501
    assert resp.json()["detail"]["error"]["code"] == "E_ACTION_NOT_SUPPORTED"


def test_listing_offers_only_actions_that_dispatch(client: TestClient) -> None:
    listed = client.get("/api/v1/peripherals").json()["peripherals"]
    assert [a["id"] for a in listed[0]["actions"]] == ["restart_radio"]
    single = client.get("/api/v1/peripherals/ados.rtl8812eu-radio").json()
    assert [a["id"] for a in single["actions"]] == ["restart_radio"]
