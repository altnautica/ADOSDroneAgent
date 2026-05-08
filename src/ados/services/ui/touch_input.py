"""Backwards-compat re-export. The implementation lives in touch.bridge.

The original module published a thin :class:`TouchInputBridge` that
turned ADS7846 pen-down events into synthetic ButtonEvent broadcasts.
The page-aware replacement at :mod:`ados.services.ui.touch.bridge`
exposes the same constructor signature plus a ``mode`` setter for
toggling between the new gesture-based path and the legacy carousel
button path. Existing imports (``from ados.services.ui.touch_input
import TouchInputBridge``) continue to work without a code change.
"""

from __future__ import annotations

from ados.services.ui.touch.bridge import TouchInputBridge  # noqa: F401

__all__ = ["TouchInputBridge"]
