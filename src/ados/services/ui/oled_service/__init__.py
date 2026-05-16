"""OLED service package — re-exports the public surface.

The original ``oled_service.py`` module was split into:

* ``service.py`` — :class:`OledService`, the asyncio service that owns
  the OLED + framebuffer + button + touch lifecycle, plus the
  ``main()`` / ``_amain()`` entry points the systemd unit invokes.
* ``constants.py`` — pin map (B1..B4), display geometry, polling
  cadences, brightness thresholds.
* ``screen_registry.py`` — :data:`SCREEN_RENDERERS`,
  :data:`OVERLAY_SCREENS`, default screen order/enabled lists.
* ``menu_tree.py`` — :data:`MENU_TREE`, :func:`_filter_visible`,
  :func:`_normalize_radio_fields`, :func:`_now`.

Existing callers (``from ados.services.ui.oled_service import
OledService``) keep working unchanged. The mesh test suite imports
:data:`MENU_TREE`, :func:`_filter_visible`, and :data:`OVERLAY_SCREENS`
directly via this package path; both remain available here.
"""

from __future__ import annotations

from .constants import (
    AUTO_CYCLE_SECONDS,
    B1,
    B2,
    B3,
    B4,
    CONTRAST_ACTIVE,
    CONTRAST_DIM,
    HEIGHT,
    IDLE_DIM_SECONDS,
    IDLE_LCD_FLOOR_HZ,
    IDLE_LCD_FLOOR_SECONDS,
    INVERT_PERIOD_SECONDS,
    PAIRING_POLL_SECONDS,
    POLL_PERIOD_SECONDS,
    WIDTH,
)
from .menu_tree import (
    MENU_TREE,
    _filter_visible,
    _normalize_radio_fields,
    _now,
)
from .screen_registry import (
    DEFAULT_SCREEN_ENABLED,
    DEFAULT_SCREEN_ORDER,
    OVERLAY_SCREENS,
    SCREEN_RENDERERS,
)
from .service import (
    OledService,
    _amain,
    log,
    main,
)

__all__ = [
    "AUTO_CYCLE_SECONDS",
    "B1",
    "B2",
    "B3",
    "B4",
    "CONTRAST_ACTIVE",
    "CONTRAST_DIM",
    "DEFAULT_SCREEN_ENABLED",
    "DEFAULT_SCREEN_ORDER",
    "HEIGHT",
    "IDLE_DIM_SECONDS",
    "IDLE_LCD_FLOOR_HZ",
    "IDLE_LCD_FLOOR_SECONDS",
    "INVERT_PERIOD_SECONDS",
    "MENU_TREE",
    "OVERLAY_SCREENS",
    "OledService",
    "PAIRING_POLL_SECONDS",
    "POLL_PERIOD_SECONDS",
    "SCREEN_RENDERERS",
    "WIDTH",
    "_amain",
    "_filter_visible",
    "_normalize_radio_fields",
    "_now",
    "log",
    "main",
]
