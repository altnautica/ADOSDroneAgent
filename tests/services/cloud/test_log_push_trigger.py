"""Tests for the thin cloud-push trigger.

The trigger only signals intent (writes the request file) and reports the
outcome (reads the result file); the cloud service does the export, upload, and
mark. These tests pin the request-file shape (the shared contract the cloud
service consumes), the ``since`` parsing, the kind validation, and the
result read-back.
"""

from __future__ import annotations

import json

import pytest

from ados.services.cloud import log_push_trigger as trig
from ados.services.cloud.log_push_trigger import (
    LogPushTriggerError,
    PushRequest,
    build_request,
    parse_since,
    validate_kinds,
)


@pytest.fixture(autouse=True)
def _redirect_paths(tmp_path, monkeypatch):
    """Point the trigger at a temp request/result pair, never ``/run/ados``."""
    req = tmp_path / "logd-push-request.json"
    res = tmp_path / "logd-push-result.json"
    monkeypatch.setattr(trig, "ADOS_RUN_DIR", tmp_path)
    monkeypatch.setattr(trig, "LOGD_PUSH_REQUEST_PATH", req)
    monkeypatch.setattr(trig, "LOGD_PUSH_RESULT_PATH", res)
    return req, res


# --- since parsing (matches the store's export filter vocabulary) -------


def test_parse_since_none_and_empty() -> None:
    assert parse_since(None) is None
    assert parse_since("") is None
    assert parse_since("   ") is None


def test_parse_since_relative_resolves_against_now() -> None:
    import time

    before = int(time.time() * 1_000_000)
    got = parse_since("-5m")
    after = int(time.time() * 1_000_000)
    assert got is not None
    # 5 minutes back from "now", bracketed by the two now-reads.
    assert before - 5 * 60_000_000 <= got <= after - 5 * 60_000_000 + 1


def test_parse_since_relative_units() -> None:
    base = parse_since("-1s")
    assert base is not None
    for spec, micros in (("-90s", 90_000_000), ("-2h", 7_200_000_000), ("-1d", 86_400_000_000), ("-500ms", 500_000)):
        assert parse_since(spec) is not None


def test_parse_since_absolute_microseconds() -> None:
    assert parse_since("1700000000000000") == 1_700_000_000_000_000


def test_parse_since_iso_utc() -> None:
    # 2023-11-14T22:13:20Z is 1700000000 s since the epoch.
    assert parse_since("2023-11-14T22:13:20Z") == 1_700_000_000_000_000
    assert parse_since("2023-11-14 22:13:20") == 1_700_000_000_000_000


def test_parse_since_rejects_garbage() -> None:
    with pytest.raises(LogPushTriggerError) as ei:
        parse_since("yesterday")
    assert ei.value.code == "bad_since"
    with pytest.raises(LogPushTriggerError) as ei2:
        parse_since("-5x")
    assert ei2.value.code == "bad_since"


# --- kind validation -----------------------------------------------------


def test_validate_kinds_empty_means_all_four() -> None:
    assert validate_kinds(None) == ["logs", "metrics", "events", "hw"]
    assert validate_kinds([]) == ["logs", "metrics", "events", "hw"]


def test_validate_kinds_subset_dedup_and_case() -> None:
    assert validate_kinds(["Logs", "hw", "logs"]) == ["logs", "hw"]


def test_validate_kinds_rejects_unknown() -> None:
    with pytest.raises(LogPushTriggerError) as ei:
        validate_kinds(["logs", "telemetry"])
    assert ei.value.code == "bad_kind"


# --- request build + file write (the shared contract) -------------------


def test_build_request_assembles_selector() -> None:
    req = build_request(session=7, since="1700000000000000", kinds=["logs"])
    assert req == PushRequest(session=7, since_us=1_700_000_000_000_000, kinds=["logs"])


def test_write_request_writes_the_contract_shape(_redirect_paths) -> None:
    req_path, _res = _redirect_paths
    req = build_request(session=3, since="-1h", kinds=["logs", "events"])
    request_id = trig.write_request(req)

    on_disk = json.loads(req_path.read_text(encoding="utf-8"))
    # The shared contract the cloud service consumes.
    assert on_disk["session"] == 3
    assert isinstance(on_disk["since_us"], int)
    assert on_disk["kinds"] == ["logs", "events"]
    assert on_disk["request_id"] == request_id
    assert isinstance(on_disk["requested_at_us"], int)


def test_write_request_clears_a_stale_result(_redirect_paths) -> None:
    req_path, res_path = _redirect_paths
    res_path.write_text(json.dumps({"request_id": "old", "pushed": True}), encoding="utf-8")
    trig.write_request(build_request(session=None, since=None, kinds=None))
    # The stale result is cleared so a poll cannot latch onto the wrong run.
    assert not res_path.exists()


def test_write_request_null_session_and_all_kinds(_redirect_paths) -> None:
    req_path, _res = _redirect_paths
    trig.write_request(build_request(session=None, since=None, kinds=None))
    on_disk = json.loads(req_path.read_text(encoding="utf-8"))
    assert on_disk["session"] is None
    assert on_disk["since_us"] is None
    assert on_disk["kinds"] == ["logs", "metrics", "events", "hw"]


# --- result read-back ----------------------------------------------------


def test_read_result_matches_request_id(_redirect_paths) -> None:
    _req, res_path = _redirect_paths
    res_path.write_text(
        json.dumps(
            {
                "request_id": "rid-1",
                "pushed": True,
                "deduped": False,
                "bytes": 2048,
                "rows": 17,
                "synced": True,
                "window_id": "w_abc",
                "sha256": "deadbeef",
            }
        ),
        encoding="utf-8",
    )
    out = trig.read_result("rid-1", timeout=0.5)
    assert out["accepted"] is True
    assert out["pushed"] is True
    assert out["bytes"] == 2048
    assert out["rows"] == 17
    assert out["synced"] is True
    assert out["window_id"] == "w_abc"
    assert out["pending"] is False
    assert out["error"] is None


def test_read_result_ignores_mismatched_id_then_times_out(_redirect_paths) -> None:
    _req, res_path = _redirect_paths
    res_path.write_text(json.dumps({"request_id": "other", "pushed": True}), encoding="utf-8")
    out = trig.read_result("rid-2", timeout=0.3)
    # No result for our id within the window → pending placeholder, push not lost.
    assert out["pending"] is True
    assert out["accepted"] is True
    assert out["pushed"] is False


def test_read_result_surfaces_an_error_from_the_cloud_service(_redirect_paths) -> None:
    _req, res_path = _redirect_paths
    res_path.write_text(
        json.dumps({"request_id": "rid-3", "pushed": False, "error": "cloud_logs_disabled"}),
        encoding="utf-8",
    )
    out = trig.read_result("rid-3", timeout=0.5)
    assert out["error"] == "cloud_logs_disabled"
    assert out["pushed"] is False


def test_trigger_push_no_wait_returns_pending(_redirect_paths) -> None:
    req_path, _res = _redirect_paths
    out = trig.trigger_push(build_request(session=None, since=None, kinds=["logs"]), wait=False)
    assert out["pending"] is True
    assert out["accepted"] is True
    # The request was still written for the cloud service to pick up.
    assert req_path.exists()
    assert out["request_id"]


def test_trigger_push_wait_reads_the_cloud_result(_redirect_paths, monkeypatch) -> None:
    _req, res_path = _redirect_paths

    # The cloud service answers under the same id the trigger picked. Stage the
    # answer the moment the request is recorded, then trigger_push polls it back.
    real_write = trig.write_request

    def write_then_answer(request):
        rid = real_write(request)
        res_path.write_text(
            json.dumps({"request_id": rid, "pushed": True, "bytes": 10, "rows": 1, "synced": True}),
            encoding="utf-8",
        )
        return rid

    monkeypatch.setattr(trig, "write_request", write_then_answer)
    out = trig.trigger_push(build_request(session=None, since=None, kinds=["logs"]), wait=True, timeout=0.5)
    assert out["pushed"] is True
    assert out["rows"] == 1
    assert out["synced"] is True
    assert out["pending"] is False
