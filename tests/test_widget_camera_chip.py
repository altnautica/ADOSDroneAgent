"""Tests for the camera switch chip."""

from __future__ import annotations

from PIL import Image

from ados.services.ui.theme import DARK
from ados.services.ui.widgets.camera_chip import HEIGHT, WIDTH, draw_camera_chip


def _canvas() -> Image.Image:
    return Image.new("RGB", (WIDTH + 16, HEIGHT + 16), DARK.bg_primary)


def test_count_one_hides_chip() -> None:
    img = _canvas()
    zone = draw_camera_chip(
        img,
        0,
        0,
        palette=DARK,
        label="CAM 1",
        count=1,
    )
    assert zone is None


def test_hidden_flag_hides_chip() -> None:
    img = _canvas()
    zone = draw_camera_chip(
        img,
        0,
        0,
        palette=DARK,
        label="CAM 1",
        count=4,
        hidden=True,
    )
    assert zone is None


def test_count_gt_one_returns_zone() -> None:
    img = _canvas()
    zone = draw_camera_chip(
        img,
        4,
        4,
        palette=DARK,
        label="CAM 1",
        count=2,
    )
    assert zone is not None
    assert zone.id == "video.cam_chip"
    assert zone.x == 4 and zone.y == 4
    assert zone.w == WIDTH and zone.h == HEIGHT


def test_paint_renders_label_pixels() -> None:
    img = _canvas()
    draw_camera_chip(
        img,
        0,
        0,
        palette=DARK,
        label="CAM 1",
        count=2,
    )
    # The chip plate is bg_secondary; with text rendered on top the
    # luminance extrema must reach into the bright range.
    extrema = img.convert("L").getextrema()
    assert extrema[1] >= 150
