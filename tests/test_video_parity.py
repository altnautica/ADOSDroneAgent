"""Value-parity between the live video reads and their logd-derived twins.

These helper-level units prove the store-derived video reads honestly degrade
to ``None`` when no producer has written samples, and that the latency source
falls back to ``"sei"`` when the source event is absent.

The store is mocked at the ``telemetry_source`` seam (``latest_metrics`` /
``query_rows``), so the helpers run their real mapping over canned store rows.
"""

from __future__ import annotations

from typing import Any

import pytest

from ados.api.sources import video as video_source


def _latency_metric_rows() -> list[dict[str, Any]]:
    # No video.latency.pipeline_ms row -> derived pipeline_latency_ms is None,
    # matching the live blob's null.
    return [
        {"metric": "video.latency.glass_ms", "value": 42.5},
        {"metric": "video.latency.ewma_ms", "value": 40.1},
        {"metric": "video.latency.samples", "value": 7.0},
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
# helper-level units (the pure mapping)
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_video_latency_source_defaults_to_sei_without_the_event(monkeypatch):
    # No video.latency_source event in the window -> the derived source falls back
    # to "sei", the historic default the live read also uses.
    _patch_store(monkeypatch, metric_rows=_latency_metric_rows(), event_rows=[])
    derived = await video_source.latest_video_latency()
    assert derived is not None
    assert derived["source"] == "sei"


@pytest.mark.asyncio
async def test_latest_air_pipeline_none_without_producer(monkeypatch):
    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    assert await video_source.latest_air_pipeline() is None


@pytest.mark.asyncio
async def test_latest_video_latency_none_without_samples(monkeypatch):
    _patch_store(monkeypatch, metric_rows=[], event_rows=[])
    assert await video_source.latest_video_latency() is None
