"""Tests for the REC button widget."""

from __future__ import annotations

from PIL import Image

from ados.services.ui.theme import DARK
from ados.services.ui.widgets.rec_button import HEIGHT, WIDTH, draw_rec_button


def _canvas() -> Image.Image:
    return Image.new("RGB", (WIDTH + 16, HEIGHT + 16), DARK.bg_primary)


def _has_color(img: Image.Image, color: tuple[int, int, int]) -> bool:
    flat = img.getcolors(maxcolors=img.size[0] * img.size[1])
    if flat is None:
        return False
    return any(c == color for _, c in flat)


def test_idle_state_returns_zone() -> None:
    img = _canvas()
    zone = draw_rec_button(img, 8, 8, palette=DARK, recording=False)
    assert zone.id == "video.rec_button"
    assert zone.x == 8 and zone.y == 8
    assert zone.w == WIDTH and zone.h == HEIGHT


def test_recording_state_paints_status_error_fill() -> None:
    img = _canvas()
    draw_rec_button(img, 8, 8, palette=DARK, recording=True, pulse_phase=0.0)
    assert _has_color(img, DARK.status_error)


def test_idle_state_does_not_paint_status_error() -> None:
    img = _canvas()
    draw_rec_button(img, 8, 8, palette=DARK, recording=False)
    assert not _has_color(img, DARK.status_error)


def test_pulse_phase_varies_dot_intensity() -> None:
    img1 = _canvas()
    img2 = _canvas()
    draw_rec_button(img1, 8, 8, palette=DARK, recording=True, pulse_phase=0.0)
    draw_rec_button(img2, 8, 8, palette=DARK, recording=True, pulse_phase=0.7)
    # Phase 0 paints the dot at full intensity; phase 0.7 dims it. The
    # pixel histograms should differ because the dot occupies several
    # pixels with different luminance.
    h1 = list(img1.convert("L").histogram())
    h2 = list(img2.convert("L").histogram())
    assert h1 != h2


def test_pulse_phase_wraps_negative_value() -> None:
    """A negative pulse phase should still produce a valid render."""
    img = _canvas()
    zone = draw_rec_button(
        img, 8, 8, palette=DARK, recording=True, pulse_phase=-0.3,
    )
    assert zone is not None
    assert _has_color(img, DARK.status_error)
