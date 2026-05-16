"""Video page package — re-exports the public surface.

The original ``video.py`` module is now a package:

* ``page.py`` — :class:`VideoPage`, the LCD page registered with the
  navigator.
* ``metrics.py`` — pure formatting helpers + the atomic JSON sidecar
  writer used by the heartbeat enricher.

External callers only import :class:`VideoPage`, so the barrel keeps
that single name on the original path.
"""

from __future__ import annotations

# ``time`` is re-exported because legacy tests reach in via
# ``video_mod.time`` and patch ``monotonic`` on it; keeping the name
# bound at the package level preserves that workflow.
import time  # noqa: F401

from .page import (
    METRICS_H,
    PAGE_H,
    PAGE_W,
    VIDEO_H,
    _METRICS_REFRESH_SECONDS,
    _TAP_INACTIVITY_TEARDOWN_SECONDS,
    _TAP_RETRY_COOLDOWN_SECONDS,
    VideoPage,
)

__all__ = [
    "METRICS_H",
    "PAGE_H",
    "PAGE_W",
    "VIDEO_H",
    "VideoPage",
]
