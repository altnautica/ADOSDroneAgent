"""Lazy loader + RGBA tinter for the tab-bar icons.

The 24x24 RGBA PNGs at ``chrome/_icons/{name}_24.png`` are produced by
``scripts/render-lcd-icons.py``. They live next to this module so the
runtime can read them via ``importlib.resources`` without touching the
filesystem layout.

API::

    img = get_icon("dashboard")   # 24x24 RGBA PIL Image
    tinted = tint(img, (0xFF, 0xFF, 0xFF))  # paint the alpha mask in white

A missing PNG (icon name typo, build skipped) returns a small fallback
glyph rather than raising — the tab bar should always render even if a
single icon is missing.
"""

from __future__ import annotations

import functools
from importlib import resources
from typing import TYPE_CHECKING

from PIL import Image, ImageDraw

if TYPE_CHECKING:  # pragma: no cover
    pass

_ICON_PIXELS = 24
_ICON_PACKAGE = "ados.services.ui.chrome._icons"
_KNOWN_ICONS: tuple[str, ...] = (
    "dashboard",
    "video",
    "settings",
    "plus",
    "link_stats",
    "hops",
)


def _fallback_icon() -> Image.Image:
    """Return a stamped ``?`` glyph used when an icon PNG is missing.

    The glyph is drawn in solid white on a transparent background so
    callers can tint it the same way as a real icon.
    """
    img = Image.new("RGBA", (_ICON_PIXELS, _ICON_PIXELS), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rectangle(
        (1, 1, _ICON_PIXELS - 2, _ICON_PIXELS - 2),
        outline=(255, 255, 255, 255),
        width=2,
    )
    draw.text((9, 4), "?", fill=(255, 255, 255, 255))
    return img


@functools.lru_cache(maxsize=16)
def get_icon(name: str) -> Image.Image:
    """Return the cached 24x24 RGBA PIL Image for ``name``.

    Falls back to a stamped ``?`` glyph if the file is missing. The
    cache is process-lifetime — icons are tiny and re-decoded only on
    the first paint per icon.
    """
    filename = f"{name}_24.png"
    try:
        with resources.files(_ICON_PACKAGE).joinpath(filename).open("rb") as fh:
            img = Image.open(fh)
            img.load()
        if img.mode != "RGBA":
            img = img.convert("RGBA")
        return img
    except (FileNotFoundError, ModuleNotFoundError, OSError):
        return _fallback_icon()


def tint(icon: Image.Image, color: tuple[int, int, int]) -> Image.Image:
    """Paint the alpha mask of ``icon`` in solid ``color``.

    The source PNG is rendered with ``stroke="currentColor"``, which
    cairosvg rasterizes as black-on-transparent. To recolor, we drop a
    solid color rectangle behind the alpha channel; the result keeps
    the original opacity profile but in any color the palette wants.
    """
    if icon.mode != "RGBA":
        icon = icon.convert("RGBA")
    out = Image.new("RGBA", icon.size, (0, 0, 0, 0))
    solid = Image.new("RGBA", icon.size, color + (255,))
    out.paste(solid, mask=icon.split()[3])
    return out


def known_icons() -> tuple[str, ...]:
    """Return the tuple of icon names this module knows how to load."""
    return _KNOWN_ICONS
