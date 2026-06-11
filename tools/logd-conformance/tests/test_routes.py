"""Static checks on the initial route registry."""

from logd_conformance.routes import Locator, initial_routes, route_by_name


def test_initial_set_has_the_expected_routes():
    names = [r.name for r in initial_routes()]
    assert names == [
        "logs",
        "hw-summary",
        "hw-snapshot",
        "service-events",
        "link-metrics",
        "video-metrics",
    ]


def test_hw_snapshot_route_uses_the_signal_locator():
    route = route_by_name("hw-snapshot")
    assert route is not None
    assert route.kind == "hw"
    assert all(f.locator == Locator.SIGNAL for f in route.fields)


def test_log_route_maps_every_legacy_entry_field():
    route = route_by_name("logs")
    assert route is not None
    legacy_map = {f.legacy_field: f.field for f in route.fields}
    # The legacy entry shape is { seq, timestamp, level, logger, message }; each
    # maps onto a store column the store serves.
    assert legacy_map == {
        "seq": "id",
        "timestamp": "ts_us",
        "level": "level",
        "logger": "source",
        "message": "msg",
    }


def test_metric_routes_use_the_metric_locator():
    for name in ("link-metrics", "video-metrics", "hw-summary"):
        route = route_by_name(name)
        assert route is not None
        assert route.kind == "metrics"
        assert all(f.locator == Locator.METRIC for f in route.fields)
        assert all(f.classification == "live" for f in route.fields)


def test_service_events_route_filters_on_kind():
    route = route_by_name("service-events")
    assert route is not None
    assert route.kind == "events"
    assert route.row_match == {"kind": "service.transition"}
    assert {f.field for f in route.fields} == {"from_state", "to_state", "reason"}
    assert all(f.locator == Locator.DETAIL_KEY for f in route.fields)


def test_unknown_route_is_none():
    assert route_by_name("does-not-exist") is None
