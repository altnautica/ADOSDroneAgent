"""Shared helpers and request models for the residual ground-station routes.

Sub-modules read these through the package object
(``ados.api.routes.ground_station``) at call time, so a test that
monkeypatches ``_require_ground_profile`` or ``_pair_manager`` there is
honoured at the call site.
"""

from __future__ import annotations

from .managers import _pair_manager
from .profile import _require_ground_profile
from .validators import _stock_confirm_token

__all__ = [
    "_require_ground_profile",
    "_pair_manager",
    "_stock_confirm_token",
]
