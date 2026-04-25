"""Ground-station profile routes.

All endpoints gate on `config.agent.profile == "ground_station"` via
`_require_ground_profile()`. Agents on the default drone profile get
404 with code `E_PROFILE_MISMATCH`.

Routes are split across six sub-modules grouped by URL prefix. The
package-level `router` aggregates them so callers can keep importing
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
from ados.api.routes.ground_station.mesh import router as _mesh_router
from ados.api.routes.ground_station.network import router as _network_router
from ados.api.routes.ground_station.pairing import router as _pairing_router
from ados.api.routes.ground_station.status import router as _status_router
from ados.api.routes.ground_station.ui import router as _ui_router
from ados.api.routes.ground_station.wfb import router as _wfb_router


router = APIRouter()
router.include_router(_status_router)
router.include_router(_wfb_router)
router.include_router(_network_router)
router.include_router(_ui_router)
router.include_router(_mesh_router)
router.include_router(_pairing_router)


__all__ = ["router"]
