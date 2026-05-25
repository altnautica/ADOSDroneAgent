"""Tests for the live `link` block on GET /video/config.

The GCS Video Link panel polls only this endpoint and reads
``config.link.*``. Without the block the panel renders dead, so these
tests assert the block is present and carries the live liveness fields
from the in-process WfbManager status (drone) or the shared wfb-stats
snapshot file (ground station, separate process).
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from ados.api.routes.video import encoder_config


def _make_app(*, wfb_status=None, config_channel=149):
    """Build a fake agent app with optional in-process WfbManager."""
    app = MagicMock()
    wfb_cfg = MagicMock()
    wfb_cfg.channel = config_channel
    wfb_cfg.band = "u-nii-1"
    wfb_cfg.mcs_index = 1
    wfb_cfg.fec_k = 8
    wfb_cfg.fec_n = 12
    wfb_cfg.tx_power_dbm = 5
    wfb_cfg.wfb_link_preset = "conservative"
    wfb_cfg.adaptive_bitrate_enabled = False
    wfb_cfg.auto_hop_enabled = True
    wfb_cfg.hop_period_seconds = 60
    app.config.video.wfb = wfb_cfg
    app.config.video.camera = MagicMock(
        bitrate_kbps=4000, width=1280, height=720, fps=30, codec="h264"
    )

    if wfb_status is None:
        app.wfb_manager.return_value = None
    else:
        mgr = MagicMock()
        mgr.get_status.return_value = wfb_status
        app.wfb_manager.return_value = mgr
    app.bitrate_controller = lambda: None
    app.hop_supervisor = lambda: None
    return app


@pytest.mark.asyncio
async def test_link_block_present_from_wfb_manager():
    """Drone path: link block carries WfbManager.get_status() fields."""
    status = {
        "tx_bytes_per_s": 512000.0,
        "valid_rx_packets_per_s": 0.0,
        "video_inbound_bytes_per_s": 0.0,
        "rx_silent_seconds": None,
        "channel_locked": True,
        "acquire_state": "locked",
        "channel": 153,
    }
    app = _make_app(wfb_status=status)
    with patch(
        "ados.api.routes.video.encoder_config.get_agent_app",
        return_value=app,
    ), patch(
        "ados.api.routes.video.encoder_config._read_state_file",
        return_value=None,
    ):
        resp = await encoder_config.get_video_config()

    assert "link" in resp
    link = resp["link"]
    assert link["tx_bytes_per_s"] == 512000.0
    assert link["channel_locked"] is True
    assert link["acquire_state"] == "locked"
    assert link["channel"] == 153


@pytest.mark.asyncio
async def test_link_block_present_from_stats_file_on_ground():
    """Ground path: no in-process manager → read the stats snapshot file."""
    snapshot = {
        "valid_rx_packets_per_s": 120.0,
        "video_inbound_bytes_per_s": 480000.0,
        "rx_silent_seconds": 1.2,
        "channel_locked": True,
        "acquire_state": "locked",
        "channel": 44,
    }
    app = _make_app(wfb_status=None, config_channel=149)
    with patch(
        "ados.api.routes.video.encoder_config.get_agent_app",
        return_value=app,
    ), patch(
        "ados.api.routes.video.encoder_config._read_state_file",
        return_value=snapshot,
    ):
        resp = await encoder_config.get_video_config()

    link = resp["link"]
    assert link["valid_rx_packets_per_s"] == 120.0
    assert link["video_inbound_bytes_per_s"] == 480000.0
    assert link["rx_silent_seconds"] == 1.2
    assert link["channel_locked"] is True
    assert link["channel"] == 44


@pytest.mark.asyncio
async def test_link_block_stable_shape_when_no_data():
    """No manager and no stats file → all fields present, channel from cfg."""
    app = _make_app(wfb_status=None, config_channel=161)
    with patch(
        "ados.api.routes.video.encoder_config.get_agent_app",
        return_value=app,
    ), patch(
        "ados.api.routes.video.encoder_config._read_state_file",
        return_value=None,
    ):
        resp = await encoder_config.get_video_config()

    link = resp["link"]
    for key in (
        "tx_bytes_per_s",
        "valid_rx_packets_per_s",
        "video_inbound_bytes_per_s",
        "rx_silent_seconds",
        "channel_locked",
        "acquire_state",
        "channel",
    ):
        assert key in link, f"missing {key}"
    # Channel falls back to the configured value.
    assert link["channel"] == 161
