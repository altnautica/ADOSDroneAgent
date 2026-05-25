"""Tests for the drone video inbound-flow watchdog.

The watchdog asserts that mediamtx's per-path bytesReceived counter keeps
advancing once a publisher exists. A frozen encoder (PID alive, publisher
present, byte counter flat) must fail the health check so the run loop
restarts the publish. A counter that climbs, a still-warming-up publisher,
and an unreadable counter must all stay healthy.
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.video.pipeline.pipeline import VideoPipeline


def _make_pipeline() -> VideoPipeline:
    """Construct a VideoPipeline with a real instance but stubbed deps."""
    cfg = MagicMock()
    cfg.recording.path = "/tmp/ados-test-rec"
    with patch(
        "ados.services.video.pipeline.pipeline.CameraManager"
    ), patch(
        "ados.services.video.pipeline.pipeline.VideoRecorder"
    ), patch(
        "ados.services.video.pipeline.pipeline.MediamtxManager"
    ):
        vp = VideoPipeline(cfg)
    # Pretend a legacy encoder is alive and a publisher has been seen.
    vp._air_pipeline = None
    vp._encoder_process = MagicMock()
    vp._encoder_process.returncode = None
    vp._first_packet_seen = True
    return vp


@pytest.mark.asyncio
async def test_flow_healthy_while_bytes_climb():
    vp = _make_pipeline()
    series = iter([1000, 5000, 9000])

    async def _read():
        return next(series, 9000)

    vp._read_mediamtx_bytes_received = _read
    # First sample seeds; subsequent climbs keep it healthy.
    assert await vp._check_inbound_flow_healthy() is True
    assert await vp._check_inbound_flow_healthy() is True
    assert await vp._check_inbound_flow_healthy() is True
    assert vp.video_inbound_bytes_per_s() > 0


@pytest.mark.asyncio
async def test_flow_stalls_when_bytes_flat():
    vp = _make_pipeline()
    vp._read_mediamtx_bytes_received = AsyncMock(return_value=4242)

    with patch(
        "ados.services.video.pipeline.pipeline._INBOUND_FLOW_STALL_SECONDS",
        0.0,
    ):
        # First call seeds the counter.
        assert await vp._check_inbound_flow_healthy() is True
        # Counter unchanged and the (zeroed) stall window has elapsed →
        # the encoder is frozen, fail the probe.
        assert await vp._check_inbound_flow_healthy() is False
    assert vp.video_inbound_bytes_per_s() == 0.0


@pytest.mark.asyncio
async def test_flow_healthy_before_first_packet():
    """No publisher yet → the watchdog stays out of the way."""
    vp = _make_pipeline()
    vp._first_packet_seen = False
    vp._read_mediamtx_bytes_received = AsyncMock(return_value=0)
    assert await vp._check_inbound_flow_healthy() is True


@pytest.mark.asyncio
async def test_flow_healthy_when_counter_unreadable():
    """An unreadable counter never forces a restart."""
    vp = _make_pipeline()
    vp._read_mediamtx_bytes_received = AsyncMock(return_value=None)
    assert await vp._check_inbound_flow_healthy() is True


@pytest.mark.asyncio
async def test_flow_healthy_for_in_process_pipeline():
    """In-process pipeline runs its own watchdog; this one yields."""
    vp = _make_pipeline()
    vp._air_pipeline = MagicMock()
    # Should short-circuit before even reading the counter.
    vp._read_mediamtx_bytes_received = AsyncMock(return_value=10)
    assert await vp._check_inbound_flow_healthy() is True
