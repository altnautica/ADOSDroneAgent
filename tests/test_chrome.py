"""Tests for the LCD chrome (top status bar + bottom tab bar).

The bars draw onto a PIL Image; we exercise them by painting onto a
canvas and asserting on the returned hit zones (tab bar) and on
selected pixel-level invariants (background fills, divider line).
"""

from __future__ import annotations

from PIL import Image

from ados.services.ui.chrome import bottom_tab_bar, top_status_bar
from ados.services.ui.chrome.icons import get_icon, known_icons, tint
from ados.services.ui.theme import DARK


def _new_canvas() -> Image.Image:
    return Image.new("RGB", (480, 320), DARK.bg_primary)


class TestTopStatusBar:
    def test_paints_within_height_band(self):
        canvas = _new_canvas()
        top_status_bar.draw(
            canvas,
            0,
            0,
            480,
            palette=DARK,
            hostname="groundnode",
            state={
                "role": {"current": "receiver", "mesh_capable": True},
                "system": {
                    "cpu_pct": 25,
                    "ram_used_mb": 1500,
                    "ram_total_mb": 4096,
                    "temp_c": 47,
                },
            },
            now_str="14:32:08",
        )
        # The bar should have painted SOMETHING on row 8 (text row),
        # i.e. at least one pixel different from bg_primary.
        row_pixels = [canvas.getpixel((x, 16)) for x in range(0, 480)]
        assert any(p != DARK.bg_primary for p in row_pixels)

    def test_paints_with_missing_state(self):
        # No role, no system: should still paint without raising.
        canvas = _new_canvas()
        top_status_bar.draw(
            canvas,
            0,
            0,
            480,
            palette=DARK,
            hostname="groundnode",
            state={},
            now_str="00:00:00",
        )

    def test_height_constant(self):
        assert top_status_bar.HEIGHT == 32


class TestBottomTabBar:
    def test_returns_four_zones_with_correct_geometry(self):
        canvas = _new_canvas()
        zones = bottom_tab_bar.draw(
            canvas,
            0,
            320 - 44,
            480,
            palette=DARK,
            active="dashboard",
        )
        assert len(zones) == 4
        # Tabs span the full width, 120 px each.
        assert zones[0].x == 0
        assert zones[0].w == 120
        assert zones[0].y == 276
        assert zones[0].h == 44
        assert zones[1].x == 120
        assert zones[2].x == 240
        assert zones[3].x == 360
        # All four ids match the spec.
        assert [z.id for z in zones] == [
            "tab.dashboard",
            "tab.video",
            "tab.settings",
            "tab.more",
        ]

    def test_active_tab_gets_accent_line(self):
        canvas = _new_canvas()
        bottom_tab_bar.draw(
            canvas,
            0,
            320 - 44,
            480,
            palette=DARK,
            active="dashboard",
            now_ms=10**9,  # large so no feedback flash
        )
        # The accent line lives on row y=276..277 over the active
        # tab (x in 0..119).
        accent_pixel = canvas.getpixel((10, 276))
        assert accent_pixel == DARK.accent_primary

    def test_tap_feedback_flash_inverts_tab(self):
        canvas = _new_canvas()
        # tapped_at_ms = now_ms means feedback_age = 0 -> linger window.
        bottom_tab_bar.draw(
            canvas,
            0,
            320 - 44,
            480,
            palette=DARK,
            active="dashboard",
            tapped_at_ms={"tab.video": 5000},
            now_ms=5000,
        )
        # A pixel inside the second tab (x in 120..239) should be the
        # text_primary color (the inverse fill).
        pix = canvas.getpixel((180, 300))
        assert pix == DARK.text_primary

    def test_page_id_for_zone_lookup(self):
        assert bottom_tab_bar.page_id_for_zone("tab.dashboard") == "dashboard"
        assert bottom_tab_bar.page_id_for_zone("tab.video") == "video"
        assert bottom_tab_bar.page_id_for_zone("tab.settings") == "settings"
        assert bottom_tab_bar.page_id_for_zone("tab.more") == "more"
        assert bottom_tab_bar.page_id_for_zone("nonsense") is None


class TestIcons:
    def test_get_icon_returns_24_rgba(self):
        for name in known_icons():
            icon = get_icon(name)
            assert icon.size == (24, 24)
            assert icon.mode == "RGBA"

    def test_get_icon_missing_returns_fallback(self):
        # An unknown icon name returns the ? fallback (still 24x24 RGBA).
        icon = get_icon("definitely-not-a-real-icon")
        assert icon.size == (24, 24)
        assert icon.mode == "RGBA"

    def test_tint_changes_solid_color(self):
        icon = get_icon("dashboard")
        tinted = tint(icon, (0xFF, 0x00, 0x00))
        # Find a fully-opaque pixel and verify it is red. Anti-aliased
        # edge pixels weight the paste against the transparent
        # background and produce intermediate red values, so the test
        # only checks fully opaque pixels.
        for y in range(24):
            for x in range(24):
                px = tinted.getpixel((x, y))
                if px[3] == 255:
                    assert px[0] == 0xFF
                    assert px[1] == 0x00
                    assert px[2] == 0x00
                    return
        raise AssertionError("dashboard icon has no fully opaque pixels")
