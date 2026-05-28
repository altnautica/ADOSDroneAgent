"""Tests for the AirPipeline bus_errors self-heal watcher.

The watcher observes the AirPipeline's cumulative bus_errors counter
over a sliding window and writes a runtime override file when the
rate exceeds the configured threshold. The override flips
``use_gst_air_pipeline`` to False so the next pipeline cycle falls
back to the bench-validated legacy bash path.

Key invariants tested:
* Below-threshold rate never writes the override.
* Above-threshold rate writes the override exactly once per process
  lifetime (idempotent).
* Window slides correctly so a quiet period after a burst doesn't
  retain the burst forever.
* Override file content captures the reason so an operator running
  ``cat /run/ados/video-encoder-override.yaml`` understands why their
  rig is on libx264.
* Override read/write helpers tolerate a missing /run/ados directory
  (dev box).
"""

from __future__ import annotations

import pytest
import yaml

from ados.services.video.air_pipeline import auto_fallback


@pytest.fixture
def override_in_tmp(tmp_path, monkeypatch):
    """Redirect the override path to a tmp file so tests don't touch
    /run/ados on the dev box. Yields the redirected Path."""
    fake_override = tmp_path / "video-encoder-override.yaml"
    monkeypatch.setattr(auto_fallback, "OVERRIDE_PATH", fake_override)
    yield fake_override
    # Cleanup is automatic via tmp_path teardown.


def test_below_threshold_never_writes_override(override_in_tmp):
    """A single bus_error trickle (5 errors in 60s) is well under the
    20-in-60s threshold and must not write the override."""
    w = auto_fallback.AirPipelineHealthWatcher(
        threshold=20, window_s=60.0
    )
    for i, count in enumerate([0, 1, 2, 3, 4, 5]):
        w.observe(count, now_s=float(i * 10))
    assert not w.would_trigger()
    triggered = w.maybe_trigger_fallback()
    assert triggered is False
    assert not override_in_tmp.exists()


def test_above_threshold_writes_override_once(override_in_tmp):
    """A sustained burst (40 new errors in 60s) crosses the threshold
    and the watcher writes the override file exactly once. Subsequent
    above-threshold observations are no-ops in the same process."""
    w = auto_fallback.AirPipelineHealthWatcher(
        threshold=20, window_s=60.0
    )
    # First snapshot at t=0 establishes the baseline; counter is 0.
    w.observe(0, now_s=0.0)
    # 60s later the counter has climbed by 40 — bus is on fire.
    w.observe(40, now_s=60.0)
    assert w.would_trigger()
    assert w.maybe_trigger_fallback() is True
    assert override_in_tmp.exists()
    payload = yaml.safe_load(override_in_tmp.read_text())
    assert payload["use_gst_air_pipeline"] is False
    assert "bus_errors increased by 40" in payload["reason"]

    # Idempotent: a second observe + trigger doesn't re-write.
    w.observe(80, now_s=120.0)
    # Snapshot the file mtime; second call should not change it.
    first_mtime = override_in_tmp.stat().st_mtime
    assert w.maybe_trigger_fallback() is False
    assert override_in_tmp.stat().st_mtime == first_mtime


def test_window_slides_so_old_burst_decays(override_in_tmp):
    """A spike that drops off the back of the window must not keep
    counting toward the threshold. This guards against false triggers
    from a long-ago transient that has since recovered."""
    w = auto_fallback.AirPipelineHealthWatcher(
        threshold=20, window_s=60.0
    )
    # Old spike: 30 errors at t=0..10, then quiet for two minutes.
    w.observe(0, now_s=0.0)
    w.observe(30, now_s=10.0)
    # Now we're at t=130s, well past the 60s window. The deque should
    # have evicted the old samples; counter looks like it's been flat
    # at 30 for the last 60s.
    w.observe(30, now_s=130.0)
    assert not w.would_trigger()
    assert w.maybe_trigger_fallback() is False
    assert not override_in_tmp.exists()


def test_is_auto_fallback_active_reads_override(override_in_tmp):
    """``is_auto_fallback_active()`` returns True iff the override
    file exists and explicitly disables the GStreamer pipeline. A
    missing file is False; a file with other content is False."""
    assert auto_fallback.is_auto_fallback_active() is False
    auto_fallback.write_auto_fallback_override("unit-test reason")
    assert auto_fallback.is_auto_fallback_active() is True
    # A file with the wrong shape — e.g. someone hand-wrote enabled —
    # should not count as an active override.
    override_in_tmp.write_text(yaml.safe_dump({"use_gst_air_pipeline": True}))
    assert auto_fallback.is_auto_fallback_active() is False


def test_clear_auto_fallback_removes_override(override_in_tmp):
    """The operator-facing clear helper removes the file so the next
    pipeline cycle attempts the GStreamer path again (e.g. after the
    operator installed the missing rockchip-gst plugin)."""
    auto_fallback.write_auto_fallback_override("test")
    assert override_in_tmp.exists()
    auto_fallback.clear_auto_fallback_override()
    assert not override_in_tmp.exists()
    # Idempotent: clearing twice is a no-op.
    auto_fallback.clear_auto_fallback_override()


def test_read_override_tolerates_garbage(override_in_tmp):
    """A corrupt YAML override file must not crash the config loader.
    The reader returns None and the caller treats it as 'no override
    active'."""
    override_in_tmp.write_text("not: valid: yaml: content: [")
    assert auto_fallback.read_override() is None
    assert auto_fallback.is_auto_fallback_active() is False


def test_write_tolerates_missing_parent_directory(tmp_path, monkeypatch):
    """On a stripped rootfs without /run/ados the write helper creates
    the parent directory before writing. Verify the parent-creation
    path works."""
    nested = tmp_path / "deeply" / "nested" / "ados" / "override.yaml"
    monkeypatch.setattr(auto_fallback, "OVERRIDE_PATH", nested)
    auto_fallback.write_auto_fallback_override("test")
    assert nested.exists()
    assert (
        yaml.safe_load(nested.read_text())["use_gst_air_pipeline"]
        is False
    )


def test_video_config_factory_honors_auto_fallback(monkeypatch, tmp_path):
    """End-to-end: when the override file says use_gst_air_pipeline
    is False, the VideoConfig default factory returns False even on
    a Rockchip board where the per-board default would be True."""
    from ados.core.config.video import (
        VideoConfig,
        _default_use_gst_air_pipeline,
    )

    # Point the override at a tmp file so we can flip it on/off.
    fake_override = tmp_path / "override.yaml"
    monkeypatch.setattr(auto_fallback, "OVERRIDE_PATH", fake_override)

    # Pretend we're on Rockchip so the per-board layer would say True.
    class _FakeBoard:
        soc = "RK3582"

    monkeypatch.setattr(
        "ados.hal.detect.detect_board", lambda *a, **kw: _FakeBoard()
    )

    # Without the override: Rockchip → True.
    assert _default_use_gst_air_pipeline() is True

    # Write the override and verify it suppresses the Rockchip default.
    auto_fallback.write_auto_fallback_override("simulated bus_errors")
    assert _default_use_gst_air_pipeline() is False

    # Construct a VideoConfig with no explicit setting — should pick
    # up the override.
    cfg = VideoConfig()
    assert cfg.use_gst_air_pipeline is False

    # Operator explicit True still wins over the override, because
    # explicit values bypass the factory entirely.
    cfg_explicit = VideoConfig(use_gst_air_pipeline=True)
    assert cfg_explicit.use_gst_air_pipeline is True
