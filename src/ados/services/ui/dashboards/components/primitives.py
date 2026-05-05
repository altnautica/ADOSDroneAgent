"""Color tokens + font cache for the LCD dashboards.

Colors mirror the ADOS design tokens published in
``web/design-tokens/tokens.css`` and the brand visual identity guide.
Naming intentionally matches the CSS variable names so a designer can
trace a swatch from the website to a dashboard pixel without a
translation step.

Fonts are loaded from the Debian-bundled DejaVu set
(``/usr/share/fonts/truetype/dejavu/``) which is present on every
Radxa OS Bookworm CLI build out of the box. We pin DejaVu rather
than vendoring brand fonts because (a) at 12-36 px on a 480x320
panel the visual difference vs Inter / JetBrains Mono is
imperceptible at 1.5-2 m viewing distance, (b) it avoids a 30 MB
font drop into the repo, and (c) it sidesteps font-licensing
tracking entirely.

When the DejaVu set is absent (very stripped Debian variant, or a
non-systemd container) we fall back to PIL's bundled bitmap default
which is ugly but functional — the dashboard still renders, just
in pixelated 8 px text.
"""

from __future__ import annotations

import functools
from pathlib import Path
from typing import Final

from PIL import ImageFont


# ──────────────────────────────────────────────────────────────────────
# Colors
# ──────────────────────────────────────────────────────────────────────

# Each value is an RGB tuple — PIL works with tuples natively, and we
# avoid hex parsing on every paint call.

BG_PRIMARY: Final[tuple[int, int, int]] = (0x00, 0x00, 0x00)
BG_SECONDARY: Final[tuple[int, int, int]] = (0x0A, 0x0A, 0x0A)
BG_TERTIARY: Final[tuple[int, int, int]] = (0x14, 0x14, 0x14)

TEXT_PRIMARY: Final[tuple[int, int, int]] = (0xFA, 0xFA, 0xFA)
TEXT_SECONDARY: Final[tuple[int, int, int]] = (0xA0, 0xA0, 0xA0)
TEXT_TERTIARY: Final[tuple[int, int, int]] = (0x66, 0x66, 0x66)

ACCENT_PRIMARY: Final[tuple[int, int, int]] = (0x3A, 0x82, 0xFF)
ACCENT_SECONDARY: Final[tuple[int, int, int]] = (0xDF, 0xF1, 0x40)

STATUS_SUCCESS: Final[tuple[int, int, int]] = (0x22, 0xC5, 0x5E)
STATUS_WARNING: Final[tuple[int, int, int]] = (0xF5, 0x9E, 0x0B)
STATUS_ERROR: Final[tuple[int, int, int]] = (0xEF, 0x44, 0x44)

BORDER_DEFAULT: Final[tuple[int, int, int]] = (0x1A, 0x1A, 0x1A)
BORDER_STRONG: Final[tuple[int, int, int]] = (0x2A, 0x2A, 0x2A)


def threshold_color(
    value: float | None,
    *,
    success_at: float,
    warning_at: float,
    direction: str = "higher_is_better",
) -> tuple[int, int, int]:
    """Return success/warning/error color based on a numeric threshold.

    ``direction`` chooses how to interpret the thresholds:

    * ``"higher_is_better"`` (default) — value >= success_at is green,
      >= warning_at is amber, otherwise red. Battery percent uses this.
    * ``"lower_is_better"`` — flipped: value <= success_at is green.
      CPU/temperature use this.

    None values render in tertiary grey so the operator sees "no data"
    rather than a misleading status color.
    """
    if value is None:
        return TEXT_TERTIARY
    if direction == "higher_is_better":
        if value >= success_at:
            return STATUS_SUCCESS
        if value >= warning_at:
            return STATUS_WARNING
        return STATUS_ERROR
    if value <= success_at:
        return STATUS_SUCCESS
    if value <= warning_at:
        return STATUS_WARNING
    return STATUS_ERROR


# ──────────────────────────────────────────────────────────────────────
# Fonts
# ──────────────────────────────────────────────────────────────────────

# DejaVu is shipped by the ``fonts-dejavu-core`` package on Debian /
# Radxa OS. Path stable across releases.
_DEJAVU_DIR = Path("/usr/share/fonts/truetype/dejavu")

_FONT_PATHS = {
    "sans_regular": _DEJAVU_DIR / "DejaVuSans.ttf",
    "sans_bold": _DEJAVU_DIR / "DejaVuSans-Bold.ttf",
    "mono_regular": _DEJAVU_DIR / "DejaVuSansMono.ttf",
    "mono_bold": _DEJAVU_DIR / "DejaVuSansMono-Bold.ttf",
}


@functools.lru_cache(maxsize=64)
def font(name: str, size: int) -> ImageFont.ImageFont:
    """Return a cached PIL font handle.

    ``name`` is one of ``sans_regular`` / ``sans_bold`` /
    ``mono_regular`` / ``mono_bold``. ``size`` is a pixel size; PIL
    uses pixel sizes for TTFs (not points) when no DPI is set.

    The cache is process-lifetime — fonts are big-ish on first load
    (a few hundred KB each) and we render at the same sizes on every
    tick.
    """
    path = _FONT_PATHS.get(name)
    if path is not None and path.exists():
        try:
            return ImageFont.truetype(str(path), size=size)
        except OSError:
            pass
    # Fallback path. PIL's default is bitmap-only and ignores ``size``
    # so the dashboard will look chunky on a system that lacks
    # DejaVu, but it WILL render.
    return ImageFont.load_default()


def text_size(
    draw_or_image,  # type: ignore[no-untyped-def]
    text: str,
    f: ImageFont.ImageFont,
) -> tuple[int, int]:
    """Return ``(width, height)`` for a string drawn with ``f``.

    Works against either a PIL ImageDraw or a PIL Image. Pillow 10
    uses ``getbbox`` (replaces the deprecated ``getsize``); we wrap
    here so callers don't have to remember the API change.
    """
    try:
        l, t, r, b = f.getbbox(text)
        return (r - l, b - t)
    except AttributeError:
        # Bitmap fallback fonts lack getbbox; use the deprecated path.
        return f.getsize(text)  # type: ignore[attr-defined]
