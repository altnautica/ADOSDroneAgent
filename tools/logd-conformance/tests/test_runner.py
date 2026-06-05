"""Deterministic dual-run checks against mocked store and legacy responses.

Every transport here is an ``httpx.MockTransport`` so the comparison logic is
exercised end-to-end with no live service and no network, on any host (macOS
included).
"""

import httpx
from logd_conformance.client import Fetcher
from logd_conformance.routes import initial_routes
from logd_conformance.runner import run_conformance, run_route


def _logd_client(rows_by_kind, reachable=True):
    def handler(request):
        if not reachable:
            return httpx.Response(503)
        if request.url.path == "/v1/query":
            kind = request.url.params.get("kind")
            data = rows_by_kind.get(kind, [])
            return httpx.Response(
                200, json={"data": data, "page": {"count": len(data)}, "meta": {}}
            )
        return httpx.Response(404)

    return httpx.Client(
        transport=httpx.MockTransport(handler), base_url="http://logd.local"
    )


def _legacy_client(log_entries):
    def handler(request):
        if request.url.path == "/api/logs":
            return httpx.Response(
                200, json={"entries": log_entries, "total": len(log_entries)}
            )
        # Observability proxy is not wired yet → 404, which the fetcher reads as
        # unreachable (None), the expected pre-cutover state.
        return httpx.Response(404)

    return httpx.Client(
        transport=httpx.MockTransport(handler), base_url="http://legacy.local"
    )


_FULL_METRICS = [
    {"metric": "link.rssi_dbm", "value": -53.0},
    {"metric": "link.snr_db", "value": 12.0},
    {"metric": "link.fec_uncorrected", "value": 0.0},
    {"metric": "video.encoder_bitrate_kbps", "value": 4000.0},
    {"metric": "video.framerate_hz", "value": 30.0},
    {"metric": "video.queue_depth_frames", "value": 0.0},
    {"metric": "video.dropped_frames_cumulative", "value": 0.0},
    {"metric": "cpu.utilization_pct", "value": 12.0},
    {"metric": "mem.available_pct", "value": 75.0},
    {"metric": "disk.used_pct", "value": 40.0},
    {"metric": "thermal.primary_c", "value": 48.0},
]

_FULL_LOGS = [
    {"id": 1, "ts_us": 123, "level": "INFO", "source": "ados-radio", "msg": "hi"}
]

_SERVICE_EVENT = {
    "id": 1,
    "ts_us": 1,
    "kind": "service.transition",
    "source": "ados-supervisor",
    "severity": "info",
    "detail": {
        "service": "ados-video",
        "from_state": "stopped",
        "to_state": "running",
        "reason": "start_ok",
    },
}

_LEGACY_LOGS = [
    {
        "seq": 1,
        "timestamp": "2026-06-05T00:00:00+00:00",
        "level": "INFO",
        "logger": "ados-radio",
        "message": "hi",
    }
]


def test_full_superset_passes_every_field():
    rows = {"logs": _FULL_LOGS, "metrics": _FULL_METRICS, "events": [_SERVICE_EVENT]}
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert report.ok
    assert report.failed == 0
    assert report.missing_producer == 0
    # The log route documents that the store is a superset of legacy: every
    # mapped legacy field was served by the legacy handler too.
    log_route = next(r for r in report.routes if r.route == "logs")
    assert log_route.legacy_reachable is True
    assert all(f.legacy_present for f in log_route.fields)


def test_missing_metrics_producer_is_reported_not_failed():
    # Store reachable, but the metrics table is empty: every metric field is a
    # missing producer, not a schema gap. The run still passes (no fail) unless
    # strict mode is requested.
    rows = {"logs": _FULL_LOGS, "metrics": [], "events": [_SERVICE_EVENT]}
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert report.ok
    assert report.failed == 0
    assert report.missing_producer >= 3  # at least the link metrics
    link = next(r for r in report.routes if r.route == "link-metrics")
    assert all(f.status == "missing-producer" for f in link.fields)

    strict = run_conformance(fetcher, initial_routes(), strict=True)
    assert not strict.ok


def test_event_detail_schema_gap_fails():
    # The supervisor emits service.transition rows but the detail lacks
    # ``reason``: a genuine schema gap → fail (not missing-producer).
    bad_event = {
        "kind": "service.transition",
        "source": "ados-supervisor",
        "detail": {"from_state": "stopped", "to_state": "running"},
    }
    rows = {"logs": _FULL_LOGS, "metrics": _FULL_METRICS, "events": [bad_event]}
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert not report.ok
    assert report.failed == 1
    svc = next(r for r in report.routes if r.route == "service-events")
    reason = next(f for f in svc.fields if f.field == "reason")
    assert reason.status == "fail"
    # The present detail keys still pass.
    assert next(f for f in svc.fields if f.field == "from_state").status == "pass"


def test_event_kind_filter_excludes_other_events():
    # An events table that has rows but none of kind service.transition: the
    # producer is not emitting our event → missing-producer, never a false pass.
    other = {"kind": "radio.lock", "detail": {"x": 1}}
    rows = {"logs": _FULL_LOGS, "metrics": _FULL_METRICS, "events": [other]}
    fetcher = Fetcher([_logd_client(rows)], _legacy_client(_LEGACY_LOGS))
    result = run_route(fetcher, next(r for r in initial_routes() if r.name == "service-events"))
    assert result.logd_rows == 1
    assert result.matched_rows == 0
    assert all(f.status == "missing-producer" for f in result.fields)


def test_unreachable_store_marks_all_missing_producer():
    fetcher = Fetcher([_logd_client({}, reachable=False)], _legacy_client(_LEGACY_LOGS))
    report = run_conformance(fetcher, initial_routes())
    assert report.failed == 0
    assert report.passed == 0
    for route in report.routes:
        assert route.logd_reachable is False
        assert all(f.status == "missing-producer" for f in route.fields)


def test_socket_first_then_tcp_fallback():
    # The first store client is unreachable (socket down); the second answers
    # (the TCP fallback). The run must still see the rows.
    rows = {"logs": _FULL_LOGS, "metrics": _FULL_METRICS, "events": [_SERVICE_EVENT]}
    fetcher = Fetcher(
        [_logd_client({}, reachable=False), _logd_client(rows)],
        _legacy_client(_LEGACY_LOGS),
    )
    report = run_conformance(fetcher, initial_routes())
    assert report.ok
    assert report.failed == 0
