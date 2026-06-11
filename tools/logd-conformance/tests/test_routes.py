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
        "wfb-status-drone",
        "wfb-status-gs",
        "wfb-history",
        "wfb-failover",
        "video-metrics",
        "air-pipeline-metrics",
        "air-pipeline-state",
        "video-latency",
    ]


def test_wfb_status_routes_filter_on_kind_and_source():
    for name, source in (
        ("wfb-status-drone", "ados-radio"),
        ("wfb-status-gs", "ados-groundlink"),
    ):
        route = route_by_name(name)
        assert route is not None
        assert route.kind == "events"
        assert route.row_match == {"kind": "link.wfb_status", "source": source}
        assert all(f.locator == Locator.DETAIL_KEY for f in route.fields)
        # The status body the route reads back carries the link-quality spine.
        fields = {f.field for f in route.fields}
        assert {"rssi_dbm", "snr_db", "bitrate_kbps", "loss_percent"} <= fields


def test_wfb_history_route_uses_the_metric_locator():
    route = route_by_name("wfb-history")
    assert route is not None
    assert route.kind == "metrics"
    assert all(f.locator == Locator.METRIC for f in route.fields)
    assert {f.field for f in route.fields} == {
        "link.rssi_dbm",
        "link.snr_db",
        "link.loss_percent",
        "link.bitrate_kbps",
    }


def test_wfb_failover_route_filters_on_kind():
    route = route_by_name("wfb-failover")
    assert route is not None
    assert route.kind == "events"
    assert route.row_match == {"kind": "wfb.pair.failover"}
    assert {f.field for f in route.fields} == {"state"}
    assert all(f.locator == Locator.DETAIL_KEY for f in route.fields)


def test_air_pipeline_state_route_filters_on_kind():
    route = route_by_name("air-pipeline-state")
    assert route is not None
    assert route.kind == "events"
    assert route.row_match == {"kind": "video.air_state"}
    assert {f.field for f in route.fields} == {
        "pipeline_state",
        "encoder_name",
        "camera_source",
    }
    assert all(f.locator == Locator.DETAIL_KEY for f in route.fields)


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
    for name in (
        "link-metrics",
        "video-metrics",
        "hw-summary",
        "air-pipeline-metrics",
        "video-latency",
    ):
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
