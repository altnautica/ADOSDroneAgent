"""Tests for the OLED carousel `link` screen.

The screen draws four lines (LINK header, RSSI, bitrate/FEC, TX +
topology summary) onto a 128x64 monochrome canvas. We don't snapshot
pixels here; we just confirm the renderer accepts the shapes the
status endpoint produces (with and without the new TX power /
topology fields) and never raises.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from ados.services.ui.screens import link as screen_link


def _draw() -> tuple[ImageDraw.ImageDraw, Image.Image]:
    img = Image.new("1", (128, 64))
    return ImageDraw.Draw(img), img


def test_renders_with_full_state():
    draw, img = _draw()
    state = {
        "link": {
            "rssi_dbm": -60,
            "bitrate_mbps": 18,
            "fec_recovered": 100,
            "fec_lost": 2,
            "channel": 161,
            "tx_power_dbm": 5,
        },
        "radio": {"topology": "host_vbus"},
    }
    screen_link.render(draw, 128, 64, state)
    # If anything actually painted, the image will have at least one
    # non-zero pixel.
    assert any(p != 0 for p in img.getdata())


def test_renders_with_missing_tx_power_falls_back_to_dashes():
    draw, _ = _draw()
    state = {
        "link": {
            "rssi_dbm": -70,
            "bitrate_mbps": 12,
            "fec_recovered": 0,
            "fec_lost": 0,
            "channel": 149,
        },
    }
    # No `radio` block at all; renderer must not crash.
    screen_link.render(draw, 128, 64, state)


def test_renders_with_powered_hub_topology():
    draw, _ = _draw()
    state = {
        "link": {
            "rssi_dbm": -55,
            "bitrate_mbps": 25,
            "fec_recovered": 5_000,
            "fec_lost": 1,
            "channel": 36,
            "tx_power_dbm": 18,
        },
        "radio": {"topology": "powered_hub"},
    }
    screen_link.render(draw, 128, 64, state)


def test_renders_with_external_5v_topology():
    draw, _ = _draw()
    state = {
        "link": {"tx_power_dbm": 25},
        "radio": {"topology": "external_5v"},
    }
    screen_link.render(draw, 128, 64, state)


def test_renders_with_unknown_topology_label_uses_placeholder():
    draw, _ = _draw()
    state = {
        "link": {"tx_power_dbm": 7},
        "radio": {"topology": "battery_pack"},  # not in the map
    }
    screen_link.render(draw, 128, 64, state)


def test_renders_with_completely_empty_state():
    draw, _ = _draw()
    # Empty dict is the worst-case shape the agent could serve during
    # an early-boot window.
    screen_link.render(draw, 128, 64, {})
