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
        "rf_unverified",
        "acquire_state",
        "channel",
    ):
        assert key in link, f"missing {key}"
    # Channel falls back to the configured value.
    assert link["channel"] == 161
    # With no snapshot at all there is no verdict to report, and a False here
    # would claim a transmit path had been proven.
    assert link["rf_unverified"] is None


@pytest.mark.asyncio
async def test_link_block_forwards_the_rf_unverified_verdict():
    """The radio's own verdict rides the link block in both directions.

    An in-process manager IS the live producer, so its reading is fresh by
    construction and needs no staleness gate.
    """
    unverified = {
        "tx_bytes_per_s": 750000.0,
        "channel_locked": False,
        "rf_unverified": True,
        "channel": 149,
    }
    app = _make_app(wfb_status=unverified)
    with patch(
        "ados.api.routes.video.encoder_config.get_agent_app",
        return_value=app,
    ), patch(
        "ados.api.routes.video.encoder_config._read_state_file",
        return_value=None,
    ):
        link = (await encoder_config.get_video_config())["link"]
    assert link["rf_unverified"] is True
    # Its other half rides the same block: injecting blind is not locked.
    assert link["channel_locked"] is False

    proven = {"channel_locked": True, "rf_unverified": False, "channel": 149}
    app = _make_app(wfb_status=proven)
    with patch(
        "ados.api.routes.video.encoder_config.get_agent_app",
        return_value=app,
    ), patch(
        "ados.api.routes.video.encoder_config._read_state_file",
        return_value=None,
    ):
        link = (await encoder_config.get_video_config())["link"]
    assert link["rf_unverified"] is False


@pytest.mark.parametrize(
    "status",
    [
        # Absent (a sidecar written before the field existed).
        {"channel_locked": True},
        # Present but not a boolean — a garbled body is no reading either.
        {"rf_unverified": "yes"},
        # An explicit null is already no reading.
        {"rf_unverified": None},
    ],
)
def test_rf_unverified_is_none_when_it_cannot_be_sourced(status):
    """Unknown reads unknown, never a confident False.

    A False would claim an unproven transmit path had been proven, which is
    the healthy-looking dead link this field exists to expose.
    """
    assert encoder_config._rf_unverified(status, fresh=True) is None
    assert encoder_config._rf_unverified(None, fresh=True) is None


def test_rf_unverified_is_none_on_a_stale_snapshot():
    """A reading older than the ceiling cannot describe the link NOW."""
    proven = {"rf_unverified": False}
    assert encoder_config._rf_unverified(proven, fresh=True) is False
    assert encoder_config._rf_unverified(proven, fresh=False) is None


@pytest.mark.asyncio
async def test_link_block_drops_a_stale_verdict_from_the_stats_file():
    """Ground path: an aged snapshot keeps its counters but loses the verdict.

    The staleness gate is scoped to the verdict; the sibling liveness counters
    keep the merge behaviour they always had.
    """
    snapshot = {
        "valid_rx_packets_per_s": 120.0,
        "channel_locked": True,
        "rf_unverified": False,
        "channel": 44,
    }
    app = _make_app(wfb_status=None, config_channel=149)
    with patch(
        "ados.api.routes.video.encoder_config.get_agent_app",
        return_value=app,
    ), patch(
        "ados.api.routes.video.encoder_config._read_state_file",
        return_value=snapshot,
    ), patch(
        "ados.api.routes.video.encoder_config._stats_age_seconds",
        return_value=30.0,
    ):
        link = (await encoder_config.get_video_config())["link"]
    assert link["rf_unverified"] is None
    assert link["valid_rx_packets_per_s"] == 120.0

    # The same body inside the ceiling still reports the real verdict.
    with patch(
        "ados.api.routes.video.encoder_config.get_agent_app",
        return_value=app,
    ), patch(
        "ados.api.routes.video.encoder_config._read_state_file",
        return_value=snapshot,
    ), patch(
        "ados.api.routes.video.encoder_config._stats_age_seconds",
        return_value=2.0,
    ):
        link = (await encoder_config.get_video_config())["link"]
    assert link["rf_unverified"] is False
