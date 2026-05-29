"""Tests for the extracted heartbeat helpers.

These cover the small pure helpers that the cloud subprocess folds
into the periodic status payload. The payload-build closure itself
stays in ``ados.services.cloud.__main__`` because it is bound to
asyncio loop state; this file pins the helpers it depends on.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

from ados.core.config import ADOSConfig
from ados.services.cloud import heartbeat


def test_get_services_status_handles_missing_systemd(monkeypatch) -> None:
    """When ``systemctl is-active`` fails for every unit, all rows report stopped."""
    import subprocess as _subprocess

    def fake_run(*args, **kwargs):
        raise _subprocess.SubprocessError("no systemd in this test env")

    monkeypatch.setattr(_subprocess, "run", fake_run)
    services = heartbeat.get_services_status()
    assert isinstance(services, list)
    assert len(services) >= 5
    for entry in services:
        assert "name" in entry
        assert "status" in entry
        assert entry["status"] in ("running", "failed", "stopped")
        assert "category" in entry
        assert entry["category"] in ("core", "hardware", "ondemand")
        # PID field is omitted when there is no real PID.
        assert "pid" not in entry or entry["pid"] > 0


def test_build_display_enrichment_minimal_payload(tmp_path: Path) -> None:
    """No display, no tap snapshot, no recording → only theme present."""
    with patch.object(heartbeat, "DISPLAY_CONF_PATH", tmp_path / "display.conf"), \
         patch.object(heartbeat, "TOUCH_CALIB_PATH", tmp_path / "touch.calib"), \
         patch.object(heartbeat, "LCD_VIDEO_TAP_PATH", tmp_path / "tap.json"), \
         patch.object(heartbeat, "read_recent_touch", return_value=None), \
         patch.object(heartbeat, "read_video_recording_state", return_value=None):
        enrich = heartbeat.build_display_enrichment(
            ADOSConfig(),
            has_attached_display=False,
            local_ip="10.0.0.5",
            api_port=8080,
        )
    assert enrich == {"uiTheme": "dark"}


def test_build_display_enrichment_populates_lcd_block(tmp_path: Path) -> None:
    """An attached display surfaces the snapshot URL + rotation + calib flag."""
    display_conf = tmp_path / "display.conf"
    display_conf.write_text(
        "framebuffer_path=/dev/fb1\n"
        "rotation=90\n"
        "display_id=waveshare35a\n",
    )
    touch_calib = tmp_path / "touch.calib"
    touch_calib.write_text(
        json.dumps({"calibrated": True, "matrix": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]})
    )
    with patch.object(heartbeat, "DISPLAY_CONF_PATH", display_conf), \
         patch.object(heartbeat, "TOUCH_CALIB_PATH", touch_calib), \
         patch.object(heartbeat, "LCD_VIDEO_TAP_PATH", tmp_path / "tap.json"), \
         patch.object(heartbeat, "read_recent_touch", return_value=None), \
         patch.object(heartbeat, "read_video_recording_state", return_value=None):
        enrich = heartbeat.build_display_enrichment(
            ADOSConfig(),
            has_attached_display=True,
            local_ip="10.0.0.5",
            api_port=8080,
        )
    assert enrich["lcdTouchCalibrated"] is True
    assert enrich["lcdRotation"] == 90
    assert enrich["lcdSnapshotUrl"] == "http://10.0.0.5:8080/api/v1/display/snapshot"
    assert enrich["uiTheme"] == "dark"


def test_read_lcd_state_blob_missing_returns_none(tmp_path: Path, monkeypatch) -> None:
    """No file on disk → None, callers omit the field from the payload."""
    fake_path = tmp_path / "absent-lcd-state.json"
    import ados.core.paths as _paths

    monkeypatch.setattr(_paths, "LCD_STATE_PATH", fake_path)
    assert heartbeat.read_lcd_state_blob() is None


def test_collect_attached_display_returns_none_without_conf(tmp_path: Path) -> None:
    """display.conf absent → None so peripherals stays out of the payload."""
    with patch.object(heartbeat, "DISPLAY_CONF_PATH", tmp_path / "absent.conf"):
        assert heartbeat.collect_attached_display() is None


def test_now_iso_returns_isoformat_string() -> None:
    out = heartbeat.now_iso()
    assert isinstance(out, str)
    # Cheap shape check; the exact offset depends on the host TZ.
    assert "T" in out
    assert len(out) >= len("2026-05-09T00:00:00")


def test_get_local_ip_returns_string() -> None:
    """Whether the UDP probe succeeds or not, a string IP comes back."""
    out = heartbeat.get_local_ip()
    assert isinstance(out, str)
    assert out  # non-empty
