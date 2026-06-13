"""Static checks on the seeded route-case registry."""

from api_conformance.route_cases import case_by_name, registered_cases


def test_registry_covers_the_migrated_routes():
    # The registry grows by a one-line append per migrated route, so this is a
    # subset check (not exact equality) — it pins that the seeded surface and the
    # wave-1 reads are all present without breaking on every future append.
    names = {c.name for c in registered_cases()}
    seeded = {
        "healthz",
        "version",
        "time",
        "status",
        "telemetry",
        "pairing-info",
        "pairing-code",
        "commands",
    }
    wave1 = {
        "fleet-enrollment",
        "fleet-peers",
        "params",
        "services",
        "signing-capability",
        "signing-require",
        "signing-counters",
        "wfb",
        "wfb-history",
        "wfb-pair",
        "wfb-pair-failover-status",
    }
    assert seeded <= names, f"missing seeded routes: {seeded - names}"
    assert wave1 <= names, f"missing wave-1 routes: {wave1 - names}"


def test_non_sandboxed_routes_are_read_only_gets():
    # The default-run set (everything not sandboxed) must be side-effect-free GET
    # reads, so a routine conformance run never mutates a live agent. Write routes
    # are present but carry require_sandbox=True (POST/PUT), skipped by default.
    for case in registered_cases():
        if case.require_sandbox:
            assert case.method in ("POST", "PUT", "DELETE")
        else:
            assert case.method == "GET"


def test_write_routes_are_sandboxed():
    # Every mutating route the front has taken over is registered for the bench
    # but sandboxed so it is not fired against a live agent by default.
    for name in ("params-write", "signing-enroll-fc", "service-restart"):
        case = case_by_name(name)
        assert case is not None and case.require_sandbox is True


def test_pairing_code_masks_the_regenerated_code():
    case = case_by_name("pairing-code")
    assert case is not None
    assert "code" in case.extra_volatile


def test_status_masks_the_health_block_numerics():
    # /api/status carries a nested health block whose cpu/temperature/memory/disk
    # readings move every read; the case must mask them so the structural shape is
    # what the two handlers are compared on.
    case = case_by_name("status")
    assert case is not None
    for key in ("cpu_percent", "temperature", "memory_percent", "disk_percent"):
        assert key in case.extra_volatile


def test_paired_variant_routes_carry_an_authorization_header():
    for name in ("status", "telemetry", "commands"):
        case = case_by_name(name)
        assert case is not None
        assert case.paired_headers is not None
        assert "authorization" in {k.lower() for k in case.paired_headers}


def test_unauthed_routes_have_no_paired_variant():
    for name in ("healthz", "version", "time", "pairing-info"):
        case = case_by_name(name)
        assert case is not None
        assert case.paired_headers is None


def test_unknown_route_is_none():
    assert case_by_name("does-not-exist") is None
