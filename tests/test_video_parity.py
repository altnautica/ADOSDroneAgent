"""Value-parity between the live video reads and their logd-derived twins.

Each video read route now reads the logging store first and falls back to the
live file read. These tests prove the two paths return the IDENTICAL response on
the migratable subset of fields, and that the live-only fields (the air
pipeline's three monotonic-clock floats, the request-scoped composite payload of
``/api/video``) are honestly excluded rather than fabricated.

The store is mocked at the ``telemetry_source`` seam (``latest_metrics`` /
``query_rows``), so the helpers run their real mapping over canned store rows.
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from ados.api.routes.video import latency as latency_route
from ados.api.sources import video as video_source

# A full air-pipeline snapshot body, matching AirPipelineStats.to_dict() plus the
# publisher's wall-clock updated_at_ms. The three monotonic floats carry real
# values here so the float-merge path is exercised.
_AIR_BLOB: dict[str, Any] = {
    "camera_source": "v4l2src",
    "encoder_name": "v4l2h264enc",
    "encoder_hw_accel": True,
    "pipeline_state": "playing",
    "started_at": 1234.5,
    "last_state_change_at": 1240.0,
    "encoder_fps": 30.0,
    "encoded_kbps": 6000.0,
    "sei_injected_count": 12,
    "udp_bytes_out": 4096,
    "last_buffer_at": 1245.0,
    "restart_count": 1,
    "tx_silent_kicks": 0,
    "bus_errors": 0,
    "cloud_branch_open": False,
    "updated_at_ms": 1717000000000,
}

# The SEI latency snapshot the drone-side tap writes.
_LATENCY_BLOB: dict[str, Any] = {
    "latency_ms": 42.5,
    "latency_ewma_ms": 40.1,
    "pipeline_latency_ms": None,
    "samples": 7,
    "source": "sei",
}


def _air_metric_rows() -> list[dict[str, Any]]:
    """A metrics page mirroring _AIR_BLOB's numerics as video.air.* rows."""
    return [
        {"metric": "video.air.encoder_fps", "value": 30.0},
        {"metric": "video.air.encoded_kbps", "value": 6000.0},
        {"metric": "video.air.sei_injected_count", "value": 12.0},
        {"metric": "video.air.udp_bytes_out", "value": 4096.0},
        {"metric": "video.air.restart_count", "value": 1.0},
        {"metric": "video.air.tx_silent_kicks", "value": 0.0},
        {"metric": "video.air.bus_errors", "value": 0.0},
        {"metric": "video.air.updated_at_ms", "value": 1717000000000.0},
        {"metric": "video.air.encoder_hw_accel", "value": 1.0},
        {"metric": "video.air.cloud_branch_open", "value": 0.0},
    ]


def _air_state_event_rows() -> list[dict[str, Any]]:
    return [
        {
            "kind": "video.air_state",
            "detail": {
                "name": "air-pipeline.json",
                "camera_source": "v4l2src",
                "encoder_name": "v4l2h264enc",
                "pipeline_state": "playing",
            },
        }
    ]


def _latency_metric_rows() -> list[dict[str, Any]]:
    # No video.latency.pipeline_ms row -> derived pipeline_latency_ms is None,
    # matching the live blob's null.
    return [
        {"metric": "video.latency.glass_ms", "value": 42.5},
        {"metric": "video.latency.ewma_ms", "value": 40.1},
        {"metric": "video.latency.samples", "value": 7.0},
    ]


def _latency_source_event_rows(source: str) -> list[dict[str, Any]]:
    return [
        {
            "kind": "video.latency_source",
            "detail": {"name": "lcd-latency.json", "source": source},
        }
    ]


def _patch_store(monkeypatch, *, metric_rows, event_rows):
    """Make latest_metrics / query_rows in the source module return canned rows.

    latest_metrics is reimplemented over the canned metric rows exactly as the
    real helper does (newest-wins by name); query_rows returns the canned events
    for an events query.
    """

    async def fake_latest_metrics(names, limit=200):
        out: dict[str, dict[str, Any]] = {}
        for row in metric_rows:
            m = row.get("metric")
            if m in names and m not in out:
                out[m] = {"value": row.get("value"), "tags": {}, "ts_us": 0}
        return out or None

    async def fake_query_rows(kind, limit, **params):
        if kind == "events":
            # The helper filters the events table by event_kind (the kind param
            # selects the table, not the event classifier).
            wanted = params.get("event_kind")
            return [r for r in event_rows if r.get("kind") == wanted] or None
        return None

    monkeypatch.setattr(video_source, "latest_metrics", fake_latest_metrics)
    monkeypatch.setattr(video_source, "query_rows", fake_query_rows)


# --------------------------------------------------------------------------- #
# air-pipeline parity
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_air_pipeline_logd_matches_live_on_migratable_subset(monkeypatch, tmp_path):
    air_path = tmp_path / "air-pipeline.json"
    air_path.write_text(json.dumps(_AIR_BLOB))
    monkeypatch.setattr("ados.core.paths.AIR_PIPELINE_STATS_PATH", air_path)

    # Live path: store returns nothing -> live file blob, unchanged.
    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    live = await latency_route.get_air_pipeline_status()
    assert live == _AIR_BLOB

    # Derived path: store serves the numerics + the state event; the route merges
    # the three monotonic floats from the live file.
    _patch_store(
        monkeypatch,
        metric_rows=_air_metric_rows(),
        event_rows=_air_state_event_rows(),
    )
    derived = await latency_route.get_air_pipeline_status()

    # Every field but the three monotonic floats is identical to the live blob.
    migratable = set(_AIR_BLOB) - {"started_at", "last_state_change_at", "last_buffer_at"}
    for key in migratable:
        assert derived[key] == _AIR_BLOB[key], key
    # The floats were merged back from the live file (present), so they match too.
    assert derived["started_at"] == 1234.5
    assert derived["last_state_change_at"] == 1240.0
    assert derived["last_buffer_at"] == 1245.0


@pytest.mark.asyncio
async def test_air_pipeline_floats_are_none_without_a_live_file(monkeypatch, tmp_path):
    # When the live file is absent, the store still serves every field but the
    # three monotonic floats, which honestly degrade to None (never fabricated).
    air_path = tmp_path / "air-pipeline.json"  # not created
    monkeypatch.setattr("ados.core.paths.AIR_PIPELINE_STATS_PATH", air_path)
    _patch_store(
        monkeypatch,
        metric_rows=_air_metric_rows(),
        event_rows=_air_state_event_rows(),
    )
    derived = await latency_route.get_air_pipeline_status()
    assert isinstance(derived, dict)
    assert derived["pipeline_state"] == "playing"
    assert derived["started_at"] is None
    assert derived["last_state_change_at"] is None
    assert derived["last_buffer_at"] is None


@pytest.mark.asyncio
async def test_air_pipeline_returns_store_when_live_float_read_raises(monkeypatch):
    # Store fresh, but the live float-merge read raises (a read/parse error the
    # live-only path turns into a 503). The route must return the store snapshot
    # with the three monotonic floats left None, which is strictly better than a
    # 503 when every other field is present.
    from fastapi import HTTPException

    _patch_store(
        monkeypatch,
        metric_rows=_air_metric_rows(),
        event_rows=_air_state_event_rows(),
    )

    def _raise():
        raise HTTPException(status_code=503, detail="air pipeline stats unavailable")

    monkeypatch.setattr(latency_route, "_read_air_pipeline_live_blob", _raise)
    derived = await latency_route.get_air_pipeline_status()
    assert isinstance(derived, dict)
    # Every store-carried field is present; the floats honestly degrade to None.
    assert derived["pipeline_state"] == "playing"
    assert derived["encoder_fps"] == 30.0
    assert derived["started_at"] is None
    assert derived["last_state_change_at"] is None
    assert derived["last_buffer_at"] is None


@pytest.mark.asyncio
async def test_air_pipeline_degrades_to_live_204_when_store_empty(monkeypatch, tmp_path):
    # Store empty AND no live file -> the route returns the live 204 Response,
    # identical to the pre-migration behavior.
    air_path = tmp_path / "air-pipeline.json"  # not created
    monkeypatch.setattr("ados.core.paths.AIR_PIPELINE_STATS_PATH", air_path)
    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    result = await latency_route.get_air_pipeline_status()
    # The live read returns a 204 Response object.
    from fastapi.responses import Response

    assert isinstance(result, Response)
    assert result.status_code == 204


# --------------------------------------------------------------------------- #
# latency parity
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_video_latency_logd_matches_live(monkeypatch, tmp_path):
    lat_path = tmp_path / "lcd-latency.json"
    lat_path.write_text(json.dumps(_LATENCY_BLOB))
    monkeypatch.setattr("ados.core.paths.LCD_LATENCY_STATS_PATH", lat_path, raising=False)

    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    live = await latency_route.get_video_latency()

    _patch_store(monkeypatch, metric_rows=_latency_metric_rows(), event_rows=[])
    derived = await latency_route.get_video_latency()

    assert derived == live
    assert derived == {
        "latency_ms": 42.5,
        "ewma_ms": 40.1,
        "pipeline_latency_ms": None,
        "samples": 7,
        "source": "sei",
    }


@pytest.mark.asyncio
async def test_video_latency_source_reflects_the_produced_event(monkeypatch, tmp_path):
    # The derived source is read off the video.latency_source event, not
    # hardcoded. A non-"sei" source value on the live file must round-trip through
    # the store path: the live read returns the file's source, the derived read
    # returns the event's source, and the two agree on the migratable fields.
    blob = dict(_LATENCY_BLOB, source="external")
    lat_path = tmp_path / "lcd-latency.json"
    lat_path.write_text(json.dumps(blob))
    monkeypatch.setattr("ados.core.paths.LCD_LATENCY_STATS_PATH", lat_path, raising=False)

    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    live = await latency_route.get_video_latency()
    assert live["source"] == "external"

    _patch_store(
        monkeypatch,
        metric_rows=_latency_metric_rows(),
        event_rows=_latency_source_event_rows("external"),
    )
    derived = await latency_route.get_video_latency()
    assert derived["source"] == "external"
    assert derived == live


@pytest.mark.asyncio
async def test_video_latency_source_defaults_to_sei_without_the_event(monkeypatch):
    # No video.latency_source event in the window -> the derived source falls back
    # to "sei", the historic default the live read also uses.
    _patch_store(monkeypatch, metric_rows=_latency_metric_rows(), event_rows=[])
    derived = await video_source.latest_video_latency()
    assert derived is not None
    assert derived["source"] == "sei"


@pytest.mark.asyncio
async def test_video_latency_degrades_identically_when_absent(monkeypatch, tmp_path):
    # No live file and no store samples -> both paths return the same
    # "unavailable" degraded form, the SEI probe-disabled state.
    lat_path = tmp_path / "lcd-latency.json"  # not created
    monkeypatch.setattr("ados.core.paths.LCD_LATENCY_STATS_PATH", lat_path, raising=False)

    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    live = await latency_route.get_video_latency()  # store empty -> live read
    assert live == {"latency_ms": None, "source": "unavailable"}


# --------------------------------------------------------------------------- #
# helper-level units (the pure mapping)
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_latest_air_pipeline_none_without_producer(monkeypatch):
    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    assert await video_source.latest_air_pipeline() is None


@pytest.mark.asyncio
async def test_latest_video_latency_none_without_samples(monkeypatch):
    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    assert await video_source.latest_video_latency() is None
