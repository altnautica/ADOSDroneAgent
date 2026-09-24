"""Ground-station profile routes.

All endpoints gate on `config.agent.profile == "ground_station"` via
`_require_ground_profile()`. Agents on the default drone profile get
404 with code `E_PROFILE_MISMATCH`.

The remaining routes live in the `ui` sub-module. The package-level
`router` aggregates it so callers can keep importing
`from ados.api.routes.ground_station import router`. All shared
helpers and Pydantic models live in `_common`. The package re-exports
both public and underscore-prefixed names so tests can monkeypatch via
`monkeypatch.setattr(gs, "_pair_manager", ...)` and the patched value
is picked up by sub-module call sites at request time.
"""

from __future__ import annotations

from fastapi import APIRouter

from ados.api.routes.ground_station import _common as _c

# Bulk re-export of every public attribute on _common (helpers, models,
# constants) so the package surface matches the pre-split module.
for _name in dir(_c):
    if _name.startswith("__"):
        continue
    globals()[_name] = getattr(_c, _name)

# Sub-router modules. Imported after the bulk re-export so any access
# they perform on the package at request time finds the helpers above.
from ados.api.routes.ground_station.ui import router as _ui_router

router = APIRouter()
router.include_router(_ui_router)


__all__ = ["router"]
