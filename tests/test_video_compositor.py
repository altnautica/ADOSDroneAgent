"""Tests for the VideoCompositor frame-blit + fallback helper."""

from __future__ import annotations

from PIL import Image

from ados.services.ui.theme import DARK
from ados.services.ui.widgets.video_compositor import VideoCompositor


def _canvas() -> Image.Image:
    return Image.new("RGB", (480, 244), DARK.bg_primary)


def _has_color(img: Image.Image, color: tuple[int, int, int]) -> bool:
    flat = img.getcolors(maxcolors=480 * 244)
    if flat is None:
        return False
    return any(c == color for _, c in flat)


def test_set_and_latest_round_trip() -> None:
    comp = VideoCompositor()
    assert comp.latest() is None
    frame = Image.new("RGB", (480, 176), (10, 20, 30))
    comp.set(frame)
    assert comp.latest() is frame


def test_paint_blits_frame_at_origin() -> None:
    canvas = _canvas()
    comp = VideoCompositor()
    frame = Image.new("RGB", (480, 176), (200, 50, 50))
    comp.paint(canvas, 0, 0, palette=DARK, frame=frame)
    # Top-left pixel of the frame region should now be the blit color.
    assert canvas.getpixel((0, 0)) == (200, 50, 50)
    assert canvas.getpixel((100, 100)) == (200, 50, 50)


def test_paint_resizes_off_spec_frame() -> None:
    canvas = _canvas()
    comp = VideoCompositor()
    frame = Image.new("RGB", (320, 100), (15, 220, 60))
    comp.paint(canvas, 0, 0, palette=DARK, frame=frame)
    # After resize, the entire 480x176 region should carry the frame
    # color (with possible per-pixel resampling drift, but the upper
    # left pixel is the same hue).
    assert canvas.getpixel((0, 0)) == (15, 220, 60)


def test_paint_falls_back_when_frame_none() -> None:
    canvas = _canvas()
    comp = VideoCompositor()
    comp.paint(canvas, 0, 0, palette=DARK, frame=None)
    # Placeholder card uses bg_secondary as the fill plate.
    assert _has_color(canvas, DARK.bg_secondary)


def test_paint_message_renders_text_pixels() -> None:
    canvas = _canvas()
    comp = VideoCompositor()
    comp.paint(
        canvas,
        0,
        0,
        palette=DARK,
        frame=None,
        message="Video pipeline unavailable",
    )
    # Text antialiases against the bg_secondary plate so the bright
    # pixel extreme is at least within the text-secondary range.
    extrema = canvas.crop((0, 0, 480, 176)).convert("L").getextrema()
    assert extrema[1] >= 100
