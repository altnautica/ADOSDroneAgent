"""Tests for the LocalVideoTap pipeline-string logic and fail-soft path."""

from __future__ import annotations

import sys
from unittest import mock

import pytest

from ados.services.video import local_tap as lt


def test_select_decoder_prefers_mpp_on_rockchip(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(lt, "gst_plugin_available", lambda name: name == "mppvideodec")
    assert lt.select_decoder("rk3582") == "mppvideodec"


def test_select_decoder_falls_through_to_rkvdec(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        lt,
        "gst_plugin_available",
        lambda name: name == "rkvdec",
    )
    assert lt.select_decoder("rk3588") == "rkvdec"


def test_select_decoder_v4l2_for_allwinner(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        lt,
        "gst_plugin_available",
        lambda name: name == "v4l2h264dec",
    )
    assert lt.select_decoder("a733") == "v4l2h264dec"


def test_select_decoder_software_fallback(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(lt, "gst_plugin_available", lambda name: False)
    assert lt.select_decoder("x86_64") == "avdec_h264"


def test_select_decoder_handles_empty_soc(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(lt, "gst_plugin_available", lambda name: False)
    assert lt.select_decoder("") == "avdec_h264"


def test_build_pipeline_string_uses_decoder() -> None:
    pipeline = lt.build_pipeline_string(
        source_url="rtsp://127.0.0.1:8554/main",
        decoder="mppvideodec",
        width=480,
        height=176,
        latency_ms=50,
    )
    assert "mppvideodec" in pipeline
    assert "appsink name=tap" in pipeline
    assert "max-buffers=2" in pipeline
    assert "drop=true" in pipeline
    assert "rtsp://127.0.0.1:8554/main" in pipeline
    assert "format=RGB,width=480,height=176" in pipeline
    assert "latency=50" in pipeline


def test_build_pipeline_string_software_uses_higher_latency() -> None:
    pipeline = lt.build_pipeline_string(
        source_url="rtsp://127.0.0.1:8554/main",
        decoder="avdec_h264",
        width=480,
        height=176,
        latency_ms=100,
    )
    assert "avdec_h264" in pipeline
    assert "latency=100" in pipeline


def test_gst_plugin_available_caches(monkeypatch: pytest.MonkeyPatch) -> None:
    """The inspector must shell out exactly once per plugin name."""
    inspector = lt._PluginInspector()
    calls: list[str] = []

    def fake(plugin: str) -> bool:
        calls.append(plugin)
        return True

    monkeypatch.setattr(inspector, "_shell_check", staticmethod(fake))
    inspector.available("mppvideodec")
    inspector.available("mppvideodec")
    assert calls == ["mppvideodec"]


@pytest.mark.asyncio
async def test_start_raises_unavailable_when_gi_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Without python3-gi, start() must raise the typed exception."""
    # Drop gi from sys.modules and stub the importer to fail.
    monkeypatch.setitem(sys.modules, "gi", None)  # type: ignore[arg-type]
    tap = lt.LocalVideoTap()
    with pytest.raises(lt.LocalVideoTapUnavailable):
        await tap.start()


@pytest.mark.asyncio
async def test_start_raises_unavailable_when_typelib_missing(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A real `gi` import without the Gst typelib must surface as unavailable."""
    fake_gi = mock.MagicMock()
    fake_gi.require_version = mock.MagicMock(side_effect=ValueError("no typelib"))
    monkeypatch.setitem(sys.modules, "gi", fake_gi)
    tap = lt.LocalVideoTap()
    with pytest.raises(lt.LocalVideoTapUnavailable):
        await tap.start()


def test_stats_shape_before_start() -> None:
    tap = lt.LocalVideoTap()
    stats = tap.stats()
    expected_keys = {
        "decoder_type",
        "fps",
        "frames_decoded",
        "frames_dropped",
        "first_frame_at",
        "ms_since_first_frame",
        "pipeline_state",
    }
    assert expected_keys.issubset(stats.keys())
    assert stats["frames_decoded"] == 0
    assert stats["pipeline_state"] == "idle"


def test_latest_frame_none_before_start() -> None:
    tap = lt.LocalVideoTap()
    assert tap.latest_frame() is None
