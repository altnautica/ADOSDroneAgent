"""Theme palette resolver for the SPI LCD dashboards.

Centralises every color a dashboard primitive can paint into two named
palettes (``DARK`` and ``LIGHT``), and exposes ``current_palette()``
which reads the operator's pick from ``/etc/ados/config.yaml`` at
``ui.theme``.

Why this module exists:

* Earlier builds wired colors as module-level constants on
  ``components/primitives.py``. That meant a theme switch required a
  service restart and a code change. The dashboard now resolves colors
  through the active palette, so flipping ``ui.theme`` from ``dark`` to
  ``light`` takes effect on the next render tick with no restart.
* PIL works with ``(r, g, b)`` tuples natively. We keep the tuple shape
  so primitives can pass palette colors straight to ImageDraw without a
  hex parse.

Public API:

* ``Palette`` — frozen dataclass of every named color the dashboards use.
* ``DARK``, ``LIGHT`` — the two built-in palette constants.
* ``get_palette(name)`` — lookup helper. Falls back to ``DARK`` on an
  unknown name and logs a single-line warning so the operator sees the
  fallback in journal output.
* ``current_palette()`` — reads the live config and returns the matching
  palette. Always returns a palette; never raises.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

import structlog

ThemeName = Literal["dark", "light"]

_log = structlog.get_logger("ados.services.ui.theme")


@dataclass(frozen=True)
class Palette:
    """RGB tuples for every named color the dashboards paint.

    Tuples (not hex strings) so PIL can consume the values directly.
    Naming mirrors the ADOS design tokens published in
    ``web/design-tokens/tokens.css``.
    """

    name: ThemeName
    bg_primary: tuple[int, int, int]
    bg_secondary: tuple[int, int, int]
    bg_tertiary: tuple[int, int, int]
    text_primary: tuple[int, int, int]
    text_secondary: tuple[int, int, int]
    text_tertiary: tuple[int, int, int]
    accent_primary: tuple[int, int, int]
    accent_secondary: tuple[int, int, int]
    border_default: tuple[int, int, int]
    border_strong: tuple[int, int, int]
    status_success: tuple[int, int, int]
    status_warning: tuple[int, int, int]
    status_error: tuple[int, int, int]


DARK = Palette(
    name="dark",
    bg_primary=(0x00, 0x00, 0x00),
    bg_secondary=(0x0A, 0x0A, 0x0A),
    bg_tertiary=(0x14, 0x14, 0x14),
    text_primary=(0xFA, 0xFA, 0xFA),
    text_secondary=(0xA0, 0xA0, 0xA0),
    text_tertiary=(0x66, 0x66, 0x66),
    accent_primary=(0x3A, 0x82, 0xFF),
    accent_secondary=(0xDF, 0xF1, 0x40),
    border_default=(0x1A, 0x1A, 0x1A),
    border_strong=(0x2A, 0x2A, 0x2A),
    status_success=(0x22, 0xC5, 0x5E),
    status_warning=(0xF5, 0x9E, 0x0B),
    status_error=(0xEF, 0x44, 0x44),
)


LIGHT = Palette(
    name="light",
    bg_primary=(0xFF, 0xFF, 0xFF),
    bg_secondary=(0xF8, 0xF8, 0xF8),
    bg_tertiary=(0xEC, 0xEC, 0xEC),
    text_primary=(0x0A, 0x0A, 0x0A),
    text_secondary=(0x4A, 0x4A, 0x4A),
    text_tertiary=(0x8A, 0x8A, 0x8A),
    accent_primary=(0x14, 0x5A, 0xE0),
    accent_secondary=(0xB8, 0xCC, 0x10),
    border_default=(0xE2, 0xE2, 0xE2),
    border_strong=(0xC9, 0xC9, 0xC9),
    status_success=(0x16, 0xA3, 0x4A),
    status_warning=(0xC2, 0x6F, 0x00),
    status_error=(0xC4, 0x1E, 0x3A),
)


_PALETTES: dict[str, Palette] = {"dark": DARK, "light": LIGHT}


def get_palette(name: ThemeName | str) -> Palette:
    """Return the palette for ``name``. Falls back to ``DARK`` on miss.

    A miss is logged at warning level so an operator who set an unknown
    theme value in config.yaml sees the fallback rather than a silent
    revert.
    """
    palette = _PALETTES.get(str(name))
    if palette is None:
        _log.warning(
            "theme_unknown_palette_fallback_to_dark",
            requested=str(name),
        )
        return DARK
    return palette


def current_palette() -> Palette:
    """Return the active palette derived from the live ADOS config.

    Reads ``ui.theme`` from ``/etc/ados/config.yaml`` (via the standard
    config loader) and returns the matching palette. Defaults to
    ``DARK`` when the config cannot be read or the key is absent. This
    function never raises so callers can use it directly inside render
    paths without try/except guards.
    """
    name: str = "dark"
    try:
        from ados.core.config import load_config

        cfg = load_config()
        ui_section = getattr(cfg, "ui", None)
        if ui_section is not None:
            theme_value = getattr(ui_section, "theme", None)
            if theme_value:
                name = str(theme_value)
    except Exception:
        # Config unreadable / not yet present / loader broken on a fresh
        # rig. Default palette is the right answer; do not raise into
        # the render loop.
        name = "dark"
    return get_palette(name)
