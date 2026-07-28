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


# Non-GET cases that are nonetheless safe to fire against a live agent because
# the route has no side effect at all. Keeping them out of the sandbox is the
# point: a 501 stub is exactly the kind of fixed-envelope response the two
# transports must agree on, and sandboxing it would stop exercising it on a
# routine run. Anything added here needs a comment on its RouteCase saying why
# it cannot mutate.
SIDE_EFFECT_FREE_NON_GETS = {
    # POST /api/can/passthrough is a deliberate 501 stub with a fixed
    # {error, message} envelope and no side effect on either transport.
    "can-passthrough",
}


def test_non_sandboxed_routes_are_side_effect_free():
    # The default-run set (everything not sandboxed) must be side-effect-free, so
    # a routine conformance run never mutates a live agent. Write routes are
    # present but carry require_sandbox=True (POST/PUT/DELETE), skipped by
    # default. A non-GET escapes the sandbox only by being named above, so a new
    # mutating route registered without require_sandbox still fails here.
    for case in registered_cases():
        if case.require_sandbox:
            assert case.method in ("POST", "PUT", "DELETE")
        elif case.method != "GET":
            assert case.name in SIDE_EFFECT_FREE_NON_GETS, (
                f"non-sandboxed {case.method} {case.path} ({case.name}) is not "
                "a documented side-effect-free route"
            )


def test_the_side_effect_free_allowlist_is_not_stale():
    # An allowlist entry that no longer matches a registered non-GET case is a
    # hole: the next mutating route with that name would be waved through.
    non_gets = {c.name for c in registered_cases() if c.method != "GET"}
    assert SIDE_EFFECT_FREE_NON_GETS <= non_gets, (
        "stale allowlist entries: "
        f"{SIDE_EFFECT_FREE_NON_GETS - non_gets}"
    )


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
