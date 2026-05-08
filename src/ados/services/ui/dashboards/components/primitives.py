"""Color tokens + font cache for the LCD dashboards.

Colors are sourced from the active theme palette (see
``ados.services.ui.theme``). The dashboard primitives historically read
module-level color constants such as ``primitives.BG_PRIMARY``; that
pattern is preserved here through a module-level ``__getattr__`` that
resolves each name against the current palette on every access. A theme
flip from ``dark`` to ``light`` therefore takes effect on the next
render tick without changing any caller and without restarting the
service.

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

from PIL import ImageFont

from ados.services.ui.theme import Palette, current_palette

# ──────────────────────────────────────────────────────────────────────
# Colors — resolved lazily from the active palette
# ──────────────────────────────────────────────────────────────────────

# Module-level color names map to attributes on the active ``Palette``.
# The mapping is the source of truth for the legacy constant API; any
# attribute access ``primitives.<NAME>`` flows through ``__getattr__``
# below and reads the live palette. Keeping the indirection in one
# table means a new color shipped on the palette only needs an entry
# here to become available as a module constant for legacy callers.
_COLOR_NAME_TO_PALETTE_ATTR: dict[str, str] = {
    "BG_PRIMARY": "bg_primary",
    "BG_SECONDARY": "bg_secondary",
    "BG_TERTIARY": "bg_tertiary",
    "TEXT_PRIMARY": "text_primary",
    "TEXT_SECONDARY": "text_secondary",
    "TEXT_TERTIARY": "text_tertiary",
    "ACCENT_PRIMARY": "accent_primary",
    "ACCENT_SECONDARY": "accent_secondary",
    "BORDER_DEFAULT": "border_default",
    "BORDER_STRONG": "border_strong",
    "STATUS_SUCCESS": "status_success",
    "STATUS_WARNING": "status_warning",
    "STATUS_ERROR": "status_error",
}


def __getattr__(name: str) -> tuple[int, int, int]:
    """Resolve a legacy color constant against the current palette.

    Dashboards historically referenced module-level constants such as
    ``primitives.BG_PRIMARY``. Those constants are no longer assigned
    at module import; they are resolved on every attribute access by
    looking up the current palette. This preserves the call sites
    while making theme changes take effect on the next render tick.

    A name outside ``_COLOR_NAME_TO_PALETTE_ATTR`` raises ``AttributeError``
    so unrelated dotted access does not silently return a tuple.
    """
    attr = _COLOR_NAME_TO_PALETTE_ATTR.get(name)
    if attr is None:
        raise AttributeError(
            f"module 'primitives' has no attribute {name!r}"
        )
    palette = current_palette()
    return getattr(palette, attr)


def threshold_color(
    value: float | None,
    *,
    success_at: float,
    warning_at: float,
    direction: str = "higher_is_better",
    palette: Palette | None = None,
) -> tuple[int, int, int]:
    """Return success/warning/error color based on a numeric threshold.

    ``direction`` chooses how to interpret the thresholds:

    * ``"higher_is_better"`` (default) — value >= success_at is green,
      >= warning_at is amber, otherwise red. Battery percent uses this.
    * ``"lower_is_better"`` — flipped: value <= success_at is green.
      CPU/temperature use this.

    None values render in tertiary grey so the operator sees "no data"
    rather than a misleading status color.

    ``palette`` overrides the active palette for the call. Defaults to
    ``current_palette()`` so existing callers do not need to change.
    """
    pal = palette if palette is not None else current_palette()
    if value is None:
        return pal.text_tertiary
    if direction == "higher_is_better":
        if value >= success_at:
            return pal.status_success
        if value >= warning_at:
            return pal.status_warning
        return pal.status_error
    if value <= success_at:
        return pal.status_success
    if value <= warning_at:
        return pal.status_warning
    return pal.status_error


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
        left, top, right, bottom = f.getbbox(text)
        return (right - left, bottom - top)
    except AttributeError:
        # Bitmap fallback fonts lack getbbox; use the deprecated path.
        return f.getsize(text)  # type: ignore[attr-defined]
