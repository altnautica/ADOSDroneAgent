"""Universal setup and onboarding API routes.

This package re-exports a single :data:`router` mounted at ``/v1/setup``
that aggregates every onboarding-wizard endpoint. The implementation now
lives in per-domain sub-modules alongside this barrel:

* ``state.py`` — status, finalize, skip, reset
* ``profile.py`` — operator profile choice
* ``hardware.py`` — hardware check + refresh
* ``display.py`` — display options, install, calibrate
* ``cloud.py`` — cloudflare tunnel + cloud posture + reboot + log stream
* ``apply.py`` — batch /apply + snapshot/rollback
* ``_restorers.py`` — per-section restore helpers used by apply
* ``_common.py`` — shared constants + helpers
* ``_models.py`` — shared request/response models

Existing callers (``from ados.api.routes.setup import router``) keep
working unchanged.
"""

from __future__ import annotations

from fastapi import APIRouter

# Re-exports so existing test patch targets (e.g.
# ``ados.api.routes.setup._reboot_after_delay`` and
# ``ados.api.routes.setup.setup_state.mark_skipped``) keep resolving
# now that the routes live in sub-modules.
from ados.setup import state as setup_state  # noqa: F401

from . import apply as _apply_mod
from . import cloud as _cloud_mod
from . import display as _display_mod
from . import hardware as _hardware_mod
from . import profile as _profile_mod
from . import state as _state_mod
from .cloud import _reboot_after_delay  # noqa: F401

router = APIRouter(prefix="/v1/setup", tags=["setup"])
router.include_router(_state_mod.router)
router.include_router(_profile_mod.router)
router.include_router(_hardware_mod.router)
router.include_router(_display_mod.router)
router.include_router(_cloud_mod.router)
router.include_router(_apply_mod.router)

__all__ = ["router", "setup_state", "_reboot_after_delay"]
