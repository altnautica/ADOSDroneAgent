"""Tests for the cloud heartbeat enrichment fields landed in C8.

The cloud subprocess runs in a different process from the API server
and the OLED service, so the enrichment helpers reach into local
side-files (display.conf, touch.calib, lcd-video-tap.json) and the
local API for video state. These tests pin the contract that:

* Each field is OMITTED when its source is unavailable (matching the
  existing pattern for ``temperature`` / ``peripherals``).
* Each field is populated with the right shape when its source is
  present.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

from ados.core.config import ADOSConfig
from ados.services.cloud import __main__ as cloud_main


def _write_display_conf(path: Path, *, rotation: int = 0) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "framebuffer_path=/dev/fb1\n"
        f"rotation={rotation}\n"
        "display_id=waveshare35a\n",
    )


def _write_touch_calib(path: Path, *, calibrated: bool, skipped: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    blob: dict = {"version": 1, "calibrated": calibrated}
    if skipped:
        blob["skipped"] = True
    if calibrated:
        blob["matrix"] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
    path.write_text(json.dumps(blob))


def _write_tap_status(path: Path, **kwargs) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {
        "active": True,
        "decoder": "mppvideodec",
        "fps": 29.5,
        "recording": False,
        "updated_at_ms": int(__import__("time").time() * 1000),
    }
    payload.update(kwargs)
    path.write_text(json.dumps(payload))


def test_enrichment_omits_lcd_fields_when_no_display(tmp_path: Path) -> None:
    """No display.conf, no calibration, no tap snapshot → minimal payload."""
    with patch.object(cloud_main, "DISPLAY_CONF_PATH", tmp_path / "display.conf"), \
         patch.object(cloud_main, "TOUCH_CALIB_PATH", tmp_path / "touch.calib"), \
         patch.object(cloud_main, "LCD_VIDEO_TAP_PATH", tmp_path / "lcd-video-tap.json"), \
         patch.object(cloud_main, "_read_recent_touch", return_value=None), \
         patch.object(cloud_main, "_read_video_recording_state", return_value=None):
        enrich = cloud_main._build_display_enrichment(
            ADOSConfig(),
            has_attached_display=False,
            local_ip="10.0.0.5",
            api_port=8080,
        )
    # Only the always-present uiTheme should be there.
    assert enrich == {"uiTheme": "dark"}
    for forbidden in (
        "lcdTouchCalibrated",
        "lcdRotation",
        "lcdSnapshotUrl",
        "lcdLastTouchAt",
        "lcdLastGesture",
        "videoLocalDecoderActive",
        "videoLocalDecoderType",
        "videoLocalDecoderFps",
        "videoRecording",
    ):
        assert forbidden not in enrich


def test_enrichment_populates_all_fields_when_sources_present(
    tmp_path: Path,
) -> None:
    """Every source available → every field populated with the right shape."""
    display_conf = tmp_path / "display.conf"
    touch_calib = tmp_path / "touch.calib"
    tap_status = tmp_path / "lcd-video-tap.json"
    _write_display_conf(display_conf, rotation=90)
    _write_touch_calib(touch_calib, calibrated=True)
    _write_tap_status(
        tap_status,
        active=True,
        decoder="mppvideodec",
        fps=29.5,
        recording=True,
    )
    fake_touch = {"t": 1_700_000_000_000, "kind": "tap", "x": 100, "y": 50}
    with patch.object(cloud_main, "DISPLAY_CONF_PATH", display_conf), \
         patch.object(cloud_main, "TOUCH_CALIB_PATH", touch_calib), \
         patch.object(cloud_main, "LCD_VIDEO_TAP_PATH", tap_status), \
         patch.object(cloud_main, "_read_recent_touch", return_value=fake_touch), \
         patch.object(cloud_main, "_read_video_recording_state", return_value=True):
        enrich = cloud_main._build_display_enrichment(
            ADOSConfig(),
            has_attached_display=True,
            local_ip="10.0.0.5",
            api_port=8080,
        )
    assert enrich["lcdTouchCalibrated"] is True
    assert enrich["lcdRotation"] == 90
    assert (
        enrich["lcdSnapshotUrl"]
        == "http://10.0.0.5:8080/api/v1/display/snapshot"
    )
    assert enrich["lcdLastTouchAt"] == 1_700_000_000_000
    assert enrich["lcdLastGesture"] == "tap"
    assert enrich["videoLocalDecoderActive"] is True
    assert enrich["videoLocalDecoderType"] == "mppvideodec"
    assert enrich["videoLocalDecoderFps"] == 29.5
    assert enrich["videoRecording"] is True
    assert enrich["uiTheme"] == "dark"


def test_enrichment_lcd_calibrated_false_with_skip_marker(
    tmp_path: Path,
) -> None:
    """Skip marker on disk produces lcdTouchCalibrated=False (not omitted)."""
    display_conf = tmp_path / "display.conf"
    touch_calib = tmp_path / "touch.calib"
    _write_display_conf(display_conf)
    _write_touch_calib(touch_calib, calibrated=False, skipped=True)
    with patch.object(cloud_main, "DISPLAY_CONF_PATH", display_conf), \
         patch.object(cloud_main, "TOUCH_CALIB_PATH", touch_calib), \
         patch.object(cloud_main, "LCD_VIDEO_TAP_PATH", tmp_path / "tap.json"), \
         patch.object(cloud_main, "_read_recent_touch", return_value=None), \
         patch.object(cloud_main, "_read_video_recording_state", return_value=None):
        enrich = cloud_main._build_display_enrichment(
            ADOSConfig(),
            has_attached_display=True,
            local_ip="10.0.0.5",
            api_port=8080,
        )
    assert enrich["lcdTouchCalibrated"] is False
    assert "lcdRotation" in enrich
    assert "lcdSnapshotUrl" in enrich


def test_enrichment_drops_stale_tap_snapshot(tmp_path: Path) -> None:
    """A tap-status snapshot older than 30 s is treated as absent."""
    tap_status = tmp_path / "lcd-video-tap.json"
    tap_status.write_text(
        json.dumps({
            "active": True,
            "decoder": "mppvideodec",
            "fps": 29.5,
            "recording": False,
            "updated_at_ms": 0,  # ancient
        })
    )
    with patch.object(cloud_main, "DISPLAY_CONF_PATH", tmp_path / "display.conf"), \
         patch.object(cloud_main, "TOUCH_CALIB_PATH", tmp_path / "touch.calib"), \
         patch.object(cloud_main, "LCD_VIDEO_TAP_PATH", tap_status), \
         patch.object(cloud_main, "_read_recent_touch", return_value=None), \
         patch.object(cloud_main, "_read_video_recording_state", return_value=None):
        enrich = cloud_main._build_display_enrichment(
            ADOSConfig(),
            has_attached_display=False,
            local_ip="10.0.0.5",
            api_port=8080,
        )
    # Stale snapshot should be omitted entirely.
    assert "videoLocalDecoderActive" not in enrich
    assert "videoLocalDecoderType" not in enrich
    assert "videoLocalDecoderFps" not in enrich


def test_enrichment_uses_light_theme_when_configured(tmp_path: Path) -> None:
    config = ADOSConfig()
    config.ui.theme = "light"
    with patch.object(cloud_main, "DISPLAY_CONF_PATH", tmp_path / "display.conf"), \
         patch.object(cloud_main, "TOUCH_CALIB_PATH", tmp_path / "touch.calib"), \
         patch.object(cloud_main, "LCD_VIDEO_TAP_PATH", tmp_path / "tap.json"), \
         patch.object(cloud_main, "_read_recent_touch", return_value=None), \
         patch.object(cloud_main, "_read_video_recording_state", return_value=None):
        enrich = cloud_main._build_display_enrichment(
            config,
            has_attached_display=False,
            local_ip="10.0.0.5",
            api_port=8080,
        )
    assert enrich["uiTheme"] == "light"
