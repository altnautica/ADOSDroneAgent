"""Tests for the ``POST /api/logs/push`` thin trigger endpoint.

The endpoint records the operator's window selector to the trigger file and
reads the cloud service's result back; it never exports, uploads, or marks
itself. The runtime double is unpaired, so the auth middleware leaves the route
open (no ``X-ADOS-Key`` required in the test).
"""

from __future__ import annotations

import json

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.services.cloud import log_push_trigger as trig
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client() -> TestClient:
    runtime = build_api_runtime(uptime_seconds=0.0)
    return TestClient(create_app(runtime))


@pytest.fixture(autouse=True)
def _redirect_paths(tmp_path, monkeypatch):
    req = tmp_path / "logd-push-request.json"
    res = tmp_path / "logd-push-result.json"
    monkeypatch.setattr(trig, "ADOS_RUN_DIR", tmp_path)
    monkeypatch.setattr(trig, "LOGD_PUSH_REQUEST_PATH", req)
    monkeypatch.setattr(trig, "LOGD_PUSH_RESULT_PATH", res)
    return req, res


def test_push_no_wait_returns_202_and_writes_request(client, _redirect_paths) -> None:
    req_path, _res = _redirect_paths
    resp = client.post("/api/logs/push", json={"session": 9, "kinds": ["logs", "hw"], "wait": False})
    assert resp.status_code == 202
    body = resp.json()
    assert body["pending"] is True
    assert body["accepted"] is True
    on_disk = json.loads(req_path.read_text())
    assert on_disk["session"] == 9
    assert on_disk["kinds"] == ["logs", "hw"]


def test_push_default_all_kinds(client, _redirect_paths) -> None:
    req_path, _res = _redirect_paths
    resp = client.post("/api/logs/push", json={"wait": False})
    assert resp.status_code == 202
    on_disk = json.loads(req_path.read_text())
    assert on_disk["session"] is None
    assert on_disk["kinds"] == ["logs", "metrics", "events", "hw"]


def test_push_wait_reads_cloud_result(client, _redirect_paths, monkeypatch) -> None:
    _req, res_path = _redirect_paths

    # The cloud service answers under the same id the trigger picks. Patch the
    # writer so the result is staged the moment the request is recorded.
    real_write = trig.write_request

    def write_then_answer(request):
        rid = real_write(request)
        res_path.write_text(
            json.dumps(
                {"request_id": rid, "pushed": True, "deduped": False, "bytes": 8192, "rows": 33, "synced": True}
            )
        )
        return rid

    monkeypatch.setattr(trig, "write_request", write_then_answer)
    resp = client.post("/api/logs/push", json={"session": 1, "kinds": ["logs"]})
    assert resp.status_code == 200
    body = resp.json()
    assert body["pushed"] is True
    assert body["rows"] == 33
    assert body["bytes"] == 8192
    assert body["synced"] is True
    assert body["pending"] is False


def test_push_wait_reports_dedup(client, _redirect_paths, monkeypatch) -> None:
    _req, res_path = _redirect_paths
    real_write = trig.write_request

    def write_then_answer(request):
        rid = real_write(request)
        res_path.write_text(
            json.dumps({"request_id": rid, "pushed": True, "deduped": True, "bytes": 8192, "rows": 33, "synced": True})
        )
        return rid

    monkeypatch.setattr(trig, "write_request", write_then_answer)
    resp = client.post("/api/logs/push", json={"session": 1, "kinds": ["logs"]})
    assert resp.status_code == 200
    assert resp.json()["deduped"] is True


def test_push_surfaces_cloud_error(client, _redirect_paths, monkeypatch) -> None:
    _req, res_path = _redirect_paths
    real_write = trig.write_request

    def write_then_answer(request):
        rid = real_write(request)
        res_path.write_text(json.dumps({"request_id": rid, "pushed": False, "error": "not_cloud_paired"}))
        return rid

    monkeypatch.setattr(trig, "write_request", write_then_answer)
    resp = client.post("/api/logs/push", json={"kinds": ["logs"]})
    # The push being refused by the cloud service is a 200 result with an error
    # field, not an HTTP failure: the trigger itself succeeded.
    assert resp.status_code == 200
    assert resp.json()["error"] == "not_cloud_paired"


def test_push_rejects_bad_since(client) -> None:
    resp = client.post("/api/logs/push", json={"since": "yesterday", "wait": False})
    assert resp.status_code == 400
    assert resp.json()["error"]["code"] == "bad_since"


def test_push_rejects_bad_kind(client) -> None:
    resp = client.post("/api/logs/push", json={"kinds": ["telemetry"], "wait": False})
    assert resp.status_code == 400
    assert resp.json()["error"]["code"] == "bad_kind"


def test_push_rejects_non_int_session(client) -> None:
    resp = client.post("/api/logs/push", json={"session": "five", "wait": False})
    assert resp.status_code == 400
    assert resp.json()["error"]["code"] == "bad_session"


def test_push_accepts_comma_string_kinds(client, _redirect_paths) -> None:
    req_path, _res = _redirect_paths
    resp = client.post("/api/logs/push", json={"kinds": "logs,events", "wait": False})
    assert resp.status_code == 202
    on_disk = json.loads(req_path.read_text())
    assert on_disk["kinds"] == ["logs", "events"]
