"""Tests for the 48 px list row widget primitive."""

from __future__ import annotations

from PIL import Image

from ados.services.ui.theme import DARK
from ados.services.ui.widgets.list_row import ROW_H, draw_list_row


def _blank() -> Image.Image:
    return Image.new("RGB", (480, ROW_H), DARK.bg_primary)


def _has_color(img: Image.Image, color: tuple[int, int, int]) -> bool:
    """Return True if any pixel in ``img`` matches ``color`` exactly."""
    flat = img.getcolors(maxcolors=480 * ROW_H)
    if flat is None:
        return False
    return any(c == color for _, c in flat)


def _has_light_pixels(img: Image.Image) -> bool:
    """Return True if the image has bright pixels (label / chevron)."""
    # Antialiased TTF text rarely yields pure (250, 250, 250) hits, so
    # check by extrema instead. PIL's text uses antialiased grayscale
    # interpolated against bg, so the brightest pixel is at the cap.
    extrema = img.convert("L").getextrema()
    return extrema[1] >= 200


def test_default_variant_paints_label_and_chevron() -> None:
    img = _blank()
    draw_list_row(
        img,
        0,
        0,
        480,
        palette=DARK,
        label="Channel",
        value="149",
    )
    # PIL antialiases TTF text, so exact-color hits are rare. Confirm
    # that some bright pixels are present (label + chevron + value).
    assert _has_light_pixels(img)


def test_toggle_variant_off_state_paints_bg_tertiary() -> None:
    img = _blank()
    draw_list_row(
        img,
        0,
        0,
        480,
        palette=DARK,
        label="Hotspot",
        variant="toggle",
        state=False,
    )
    # The track fill is bg_tertiary in the off state.
    assert _has_color(img, DARK.bg_tertiary)
    # The knob is filled in text_primary; antialiased text labels would
    # never produce a perfectly-text_primary pixel, but the knob is a
    # filled ellipse and does.
    assert _has_color(img, DARK.text_primary)


def test_toggle_variant_on_state_paints_accent_primary() -> None:
    img = _blank()
    draw_list_row(
        img,
        0,
        0,
        480,
        palette=DARK,
        label="Hotspot",
        variant="toggle",
        state=True,
    )
    assert _has_color(img, DARK.accent_primary)


def test_action_variant_paints_status_text() -> None:
    img = _blank()
    draw_list_row(
        img,
        0,
        0,
        480,
        palette=DARK,
        label="Reboot",
        variant="action",
        state="armed",
    )
    # We just check that any bright pixel landed (label + status text
    # rendered). Antialiased text rarely produces an exact palette
    # color match.
    assert _has_light_pixels(img)


def test_divider_below_paints_border() -> None:
    img = _blank()
    draw_list_row(
        img,
        0,
        0,
        480,
        palette=DARK,
        label="Theme",
        value="dark",
        divider_below=True,
    )
    assert _has_color(img, DARK.border_default)


def test_divider_off_skips_border() -> None:
    _blank()
    # Force a color background so the absence of border_default is meaningful.
    base = Image.new("RGB", (480, ROW_H), (128, 128, 128))
    draw_list_row(
        base,
        0,
        0,
        480,
        palette=DARK,
        label="Theme",
        value="dark",
        divider_below=False,
    )
    assert not _has_color(base, DARK.border_default)


def test_icon_placeholder_does_not_overflow() -> None:
    img = _blank()
    draw_list_row(
        img,
        0,
        0,
        480,
        palette=DARK,
        label="Wi-Fi",
        value="ADOS-AP",
        icon_name="wifi",
    )
    # Just confirm we drew bright pixels — icon path was exercised
    # without raising and the label still rendered.
    assert _has_light_pixels(img)
