"""Deterministic checks on the dual-run comparison over synthetic response pairs.

Every Probe here is built by hand (no live service, no network), so the
comparison logic is exercised end-to-end on any host. The three canonical cases
the harness must get right are covered explicitly: an equal pair passes, a pair
differing in a non-volatile field fails, and a pair differing only in a volatile
field passes.
"""

import httpx
from api_conformance.client import Clients, Probe
from api_conformance.route_cases import RouteCase
from api_conformance.runner import (
    assert_response_equal,
    run_case,
    run_conformance,
)

_JSON = "application/json"


def _probe(status=200, body=b"", headers=None, frames=None):
    return Probe(
        ok=True,
        status=status,
        headers=headers or {"content-type": _JSON},
        body=body,
        frames=frames or [],
    )


_STATUS_CASE = RouteCase(name="status", method="GET", path="/api/status")


def test_equal_pair_passes():
    body = b'{"profile":"drone","ts":1}'
    mismatches = assert_response_equal(
        _probe(body=body), _probe(body=b'{"ts":99,"profile":"drone"}'), _STATUS_CASE
    )
    # Differs only in ts (volatile) and key order (canonicalized) → equal.
    assert mismatches == []


def test_pair_differing_in_non_volatile_field_fails():
    a = _probe(body=b'{"profile":"drone","ts":1}')
    b = _probe(body=b'{"profile":"ground","ts":1}')
    mismatches = assert_response_equal(a, b, _STATUS_CASE)
    assert any(m.kind == "body" for m in mismatches)


def test_pair_differing_only_in_volatile_field_passes():
    a = _probe(body=b'{"profile":"drone","uptime":10,"started_at":5}')
    b = _probe(body=b'{"profile":"drone","uptime":9999,"started_at":42}')
    mismatches = assert_response_equal(a, b, _STATUS_CASE)
    assert mismatches == []


def test_status_code_mismatch_is_reported():
    a = _probe(status=200, body=b"{}")
    b = _probe(status=503, body=b"{}")
    mismatches = assert_response_equal(a, b, _STATUS_CASE)
    assert any(m.kind == "status" for m in mismatches)


def test_header_mismatch_is_reported():
    a = _probe(body=b"{}", headers={"content-type": _JSON, "cache-control": "no-store"})
    b = _probe(body=b"{}", headers={"content-type": _JSON, "cache-control": "no-cache"})
    mismatches = assert_response_equal(a, b, _STATUS_CASE)
    assert any(m.kind == "header" for m in mismatches)


def test_unreachable_side_short_circuits_to_reachability():
    a = _probe(body=b"{}")
    b = Probe(ok=False, error="connect refused")
    mismatches = assert_response_equal(a, b, _STATUS_CASE)
    assert len(mismatches) == 1
    assert mismatches[0].kind == "reachability"


def test_sse_frame_sequence_equal_passes():
    case = RouteCase(name="evt", method="GET", path="/api/events", is_sse=True)
    a = _probe(frames=['data: {"v":1,"ts":1}', 'data: {"v":2,"ts":2}'])
    b = _probe(frames=['data: {"v":1,"ts":9}', 'data: {"v":2,"ts":8}'])
    # Same payloads, moving ts → equal after masking.
    assert assert_response_equal(a, b, case) == []


def test_sse_frame_sequence_difference_fails():
    case = RouteCase(name="evt", method="GET", path="/api/events", is_sse=True)
    a = _probe(frames=['data: {"v":1}', 'data: {"v":2}'])
    b = _probe(frames=['data: {"v":1}', 'data: {"v":3}'])
    mismatches = assert_response_equal(a, b, case)
    assert any(m.kind == "frames" for m in mismatches)


# --- end-to-end runs through Clients backed by MockTransport ----------------


def _mock_clients(native_handler, python_handler):
    native = httpx.Client(
        transport=httpx.MockTransport(native_handler), base_url="http://native.local"
    )
    python = httpx.Client(
        transport=httpx.MockTransport(python_handler), base_url="http://python.local"
    )
    return Clients(native, python)


def test_run_case_passes_when_both_transports_agree():
    def handler(request):
        return httpx.Response(200, json={"profile": "drone", "uptime": 1})

    clients = _mock_clients(handler, lambda r: httpx.Response(200, json={"profile": "drone", "uptime": 999}))
    result = run_case(clients, _STATUS_CASE)
    assert result.status == "pass"
    assert all(v.status == "pass" for v in result.variants)


def test_run_case_fails_on_real_body_difference():
    clients = _mock_clients(
        lambda r: httpx.Response(200, json={"profile": "drone"}),
        lambda r: httpx.Response(200, json={"profile": "ground"}),
    )
    result = run_case(clients, _STATUS_CASE)
    assert result.status == "fail"


def test_run_case_diffs_both_header_variants():
    seen = []

    def native_handler(request):
        seen.append(request.headers.get("authorization"))
        return httpx.Response(200, json={"profile": "drone"})

    clients = _mock_clients(
        native_handler, lambda r: httpx.Response(200, json={"profile": "drone"})
    )
    case = RouteCase(
        name="status",
        method="GET",
        path="/api/status",
        paired_headers={"authorization": "Bearer x"},
    )
    result = run_case(clients, case)
    assert {v.variant for v in result.variants} == {"unpaired", "paired"}
    assert result.status == "pass"
    # The paired variant actually carried the Authorization header.
    assert "Bearer x" in seen


def test_sandboxed_case_is_skipped_without_requests():
    fired = []
    clients = _mock_clients(
        lambda r: fired.append(1) or httpx.Response(200), lambda r: httpx.Response(200)
    )
    case = RouteCase(
        name="reboot", method="POST", path="/api/reboot", require_sandbox=True
    )
    result = run_case(clients, case)
    assert result.status == "skipped"
    assert result.sandboxed is True
    assert not fired  # no request ever issued against a live agent


def test_run_conformance_strict_fails_on_skipped():
    clients = _mock_clients(
        lambda r: httpx.Response(200, json={"a": 1}),
        lambda r: httpx.Response(200, json={"a": 1}),
    )
    cases = [
        RouteCase(name="ok", method="GET", path="/a"),
        RouteCase(name="sandboxed", method="POST", path="/b", require_sandbox=True),
    ]
    lenient = run_conformance(clients, cases)
    assert lenient.ok  # a skip does not fail a default run
    assert lenient.skipped == 1
    strict = run_conformance(clients, cases, strict=True)
    assert not strict.ok  # strict demands every listed route was exercised


def test_run_conformance_reports_unreachable_python_as_fail():
    def native_handler(request):
        return httpx.Response(200, json={"a": 1})

    native = httpx.Client(
        transport=httpx.MockTransport(native_handler), base_url="http://native.local"
    )
    # No python client at all → every route's python side is unreachable.
    clients = Clients(native, None)
    report = run_conformance(clients, [_STATUS_CASE])
    assert not report.ok
    assert report.failed == 1
    route = report.routes[0]
    variant = route.variants[0]
    assert variant.python_reachable is False
    assert any(m.kind == "reachability" for m in variant.mismatches)
