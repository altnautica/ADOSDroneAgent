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


def test_seeded_routes_are_all_read_only_and_not_sandboxed():
    for case in registered_cases():
        assert case.method == "GET"
        assert case.require_sandbox is False


def test_pairing_code_masks_the_regenerated_code():
    case = case_by_name("pairing-code")
    assert case is not None
    assert "code" in case.extra_volatile


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
