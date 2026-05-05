"""Render tests + snapshot for the SPI LCD dashboard.

The dashboard is a pure function (state dict -> PIL.Image) so the
test surface is small: assert the image is the right size, RGB
mode, has multiple distinct colors (= things actually got painted),
and walks all the empty-state branches without raising. We also
write the rendered image to ``tests/snapshots/`` for visual review.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from PIL import Image

from ados.services.ui.dashboards.groundnode_landscape import (
    CANVAS_H,
    CANVAS_W,
    _mock_state,
    render,
)


SNAPSHOT_DIR = Path(__file__).resolve().parent / "snapshots"
SNAPSHOT_DIR.mkdir(parents=True, exist_ok=True)


def _save_snapshot(img: Image.Image, name: str) -> Path:
    """Write a snapshot for visual review. Doesn't fail tests."""
    out = SNAPSHOT_DIR / f"{name}.png"
    img.save(out, "PNG")
    return out


class TestDashboardRender:
    def test_returns_canvas_at_native_size(self):
        img = render(_mock_state(), hostname="groundnode", now_str="13:47:23")
        assert img.size == (CANVAS_W, CANVAS_H)
        assert img.mode == "RGB"

    def test_paints_more_than_solid_background(self):
        img = render(_mock_state(), hostname="groundnode", now_str="13:47:23")
        # If the dashboard rendered actual pixels we expect dozens of
        # distinct colors (text antialiasing + status colors + tile
        # borders). Solid-black would be 1.
        colors = img.getcolors(maxcolors=200_000)
        assert colors is not None and len(colors) > 50

    def test_writes_a_snapshot_for_review(self):
        img = render(_mock_state(), hostname="groundnode", now_str="13:47:23")
        path = _save_snapshot(img, "groundnode_dashboard_default")
        assert path.exists() and path.stat().st_size > 1000


class TestEmptyStateRenders:
    """Walk the placeholder branches — no drone paired, mesh down, etc."""

    def test_no_drone_paired(self):
        state = _mock_state()
        state["drone"] = {
            "device_id": None,
            "fc_mode": None,
            "battery_pct": None,
            "gps_sats": None,
        }
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_no_drone")
        assert img.size == (CANVAS_W, CANVAS_H)

    def test_mesh_down(self):
        state = _mock_state()
        state["mesh"]["up"] = False
        state["mesh"]["peer_count"] = 0
        state["mesh"]["selected_gateway"] = None
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_mesh_down")
        assert img.size == (CANVAS_W, CANVAS_H)

    def test_direct_role_no_mesh(self):
        state = _mock_state()
        state["role"]["current"] = "direct"
        state["role"]["mesh_capable"] = False
        state["mesh"] = {}
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_direct_role")
        assert img.size == (CANVAS_W, CANVAS_H)

    def test_no_uplink(self):
        state = _mock_state()
        state["network"]["uplink_type"] = "none"
        state["network"]["uplink_reachable"] = False
        state["cloud"]["paired"] = False
        state["cloud"]["pair_code"] = "ABCDEF"
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_no_uplink")
        assert img.size == (CANVAS_W, CANVAS_H)

    def test_no_radio_link(self):
        state = _mock_state()
        state["link"] = {
            "rssi_dbm": None,
            "bitrate_mbps": None,
            "fec_recovered": None,
            "fec_lost": None,
            "channel": None,
        }
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_no_link")
        assert img.size == (CANVAS_W, CANVAS_H)

    def test_completely_empty_state_does_not_crash(self):
        """Defensive: agent might serve an almost-empty status dict on first boot."""
        img = render({}, hostname="groundnode", now_str="00:00:00")
        _save_snapshot(img, "groundnode_dashboard_empty")
        assert img.size == (CANVAS_W, CANVAS_H)


class TestThresholdColors:
    def test_low_battery_renders_without_error(self):
        state = _mock_state()
        state["drone"]["battery_pct"] = 12  # critical
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_low_battery")
        assert img.size == (CANVAS_W, CANVAS_H)

    def test_high_temp_renders_without_error(self):
        state = _mock_state()
        state["system"]["temp_c"] = 82  # past warning
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_hot_box")
        assert img.size == (CANVAS_W, CANVAS_H)

    def test_weak_rssi_renders_without_error(self):
        state = _mock_state()
        state["link"]["rssi_dbm"] = -85  # red zone
        img = render(state, hostname="groundnode", now_str="13:47:23")
        _save_snapshot(img, "groundnode_dashboard_weak_rssi")
        assert img.size == (CANVAS_W, CANVAS_H)
