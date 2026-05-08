#!/usr/bin/env python3
"""Rasterize LCD tab-bar SVG icons to 24 px PNGs.

Reads the source SVGs at ``assets/lcd-icons/*.svg`` and writes 24x24
RGBA PNGs to ``src/ados/services/ui/chrome/_icons/{name}_24.png``. The
runtime always loads the rasterized PNGs at import time so the icons
are available even on systems without cairosvg.

The script is idempotent: rerunning it on unchanged SVGs produces
byte-identical PNGs. It is intentionally a manual-build step (not part
of the pip install) because cairosvg pulls in a libcairo build that
is not needed at agent runtime.

Usage::

    uv run python scripts/render-lcd-icons.py

The icon-rendering extra ships the cairosvg dependency::

    uv pip install -e ".[icon-tools]"
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SVG_DIR = REPO_ROOT / "assets" / "lcd-icons"
PNG_DIR = REPO_ROOT / "src" / "ados" / "services" / "ui" / "chrome" / "_icons"

ICONS = ("dashboard", "video", "settings", "plus")
ICON_PIXELS = 24


def _ensure_cairosvg() -> object:
    """Return the cairosvg module or print a clear error and exit."""
    try:
        import cairosvg  # type: ignore[import-not-found]
    except ImportError:
        sys.stderr.write(
            "render-lcd-icons.py needs cairosvg. Install with:\n"
            "  uv pip install -e \".[icon-tools]\"\n"
        )
        raise SystemExit(2)
    return cairosvg


def render_icon(cairosvg_mod: object, name: str) -> Path:
    """Render ``<SVG_DIR>/<name>.svg`` to ``<PNG_DIR>/<name>_24.png``.

    Returns the output path. Creates the output directory if absent.
    """
    src = SVG_DIR / f"{name}.svg"
    if not src.exists():
        raise FileNotFoundError(f"missing source svg: {src}")
    PNG_DIR.mkdir(parents=True, exist_ok=True)
    dst = PNG_DIR / f"{name}_24.png"
    svg_bytes = src.read_bytes()
    # cairosvg.svg2png writes the PNG directly to the output path.
    # output_width + output_height force the rasterization size
    # regardless of any width/height attribute on the SVG element.
    cairosvg_mod.svg2png(  # type: ignore[attr-defined]
        bytestring=svg_bytes,
        write_to=str(dst),
        output_width=ICON_PIXELS,
        output_height=ICON_PIXELS,
    )
    return dst


def main() -> int:
    cairosvg_mod = _ensure_cairosvg()
    written: list[Path] = []
    for name in ICONS:
        path = render_icon(cairosvg_mod, name)
        written.append(path)
        print(f"rendered {path.relative_to(REPO_ROOT)} ({ICON_PIXELS}x{ICON_PIXELS})")
    print(f"done — wrote {len(written)} icons to {PNG_DIR.relative_to(REPO_ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
