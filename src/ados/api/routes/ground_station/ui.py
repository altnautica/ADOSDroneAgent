"""Ground-station UI surfaces.

Covers:
* /ui (OLED, buttons, screens persisted UI config)
* /factory-reset (pair + mesh wipe with fingerprint confirm)
* /display (HDMI kiosk config)

The PIC claim/release/confirm-token/heartbeat REST routes, the gamepad +
Bluetooth read/write routes, and the ``/pic/events`` WebSocket relay of the
arbiter's transition stream are served natively by ``ados-control`` (the Rust
``ados-pic`` / ``ados-input`` daemons own the arbiter + input state).
"""

from __future__ import annotations

from typing import Any

import structlog
from fastapi import (
    APIRouter,
    HTTPException,
    Query,
    Request,
)

from ados.api.routes import ground_station as _gs
from ados.core.paths import MESH_GATEWAY_JSON

log = structlog.get_logger("ground_station.ui")

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


# ---------------------------------------------------------------------------
# /ui
# ---------------------------------------------------------------------------


@router.get("/ui")
async def get_ground_station_ui() -> dict[str, Any]:
    """Return the full UI config (OLED, buttons, screens)."""
    _gs._require_ground_profile()
    return _gs._load_ui_config()


# ---------------------------------------------------------------------------
# /factory-reset
# ---------------------------------------------------------------------------


@router.post("/factory-reset")
async def post_factory_reset(
    request: Request,
    confirm: str = Query(..., description="Current pair key fingerprint or stock token"),
) -> dict:
    """Wipe pair state and AP passphrase. Requires the current fingerprint.

    When the ground station is paired, the confirm token must match the
    active pair key fingerprint. When unpaired, the token must match
    `factory-reset-unpaired`. This stops a casual curl from bricking a
    live device.
    """
    _gs._require_ground_profile()

    # Captive-portal single-use token check. Only factory reset is
    # gated. The header is optional when called from loopback to keep
    # CLI test paths open.
    captive_header = request.headers.get("x-ados-captive-key")
    client_host = request.client.host if request.client else None
    if client_host not in ("127.0.0.1", "::1"):
        from ados.services.setup_webapp.captive_token import get_captive_token_store

        if not captive_header or not get_captive_token_store().consume(captive_header):
            raise HTTPException(
                status_code=401,
                detail={"error": {"code": "E_CAPTIVE_TOKEN_INVALID"}},
            )

    pm = _gs._pair_manager()

    try:
        current = await pm.status("gs")
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_PAIR_STATUS_FAILED", "message": str(exc)}},
        ) from exc

    expected = current.get("fingerprint") or _gs._stock_confirm_token()
    if confirm != expected:
        raise HTTPException(
            status_code=400,
            detail={"error": {"code": "E_CONFIRM_MISMATCH"}},
        )

    # Take the node out of any mesh role BEFORE wiping pair state, so
    # mesh services (batman, wfb_relay, wfb_receiver) are stopped and
    # their identity files are not racing with the pair_manager reset.
    # `apply_role("direct")` is a no-op when already direct.
    #
    # Role transition MUST succeed before pair state is wiped. A
    # half-finished factory reset that wipes `/etc/ados/mesh/` while
    # batman-adv is still reading from it leaves the filesystem in a
    # state that usually needs a reboot to recover. On transition
    # failure we 500 and abort the whole reset. The operator can try
    # again once the services are stopped (role switched back to direct
    # manually, then retry factory reset).
    from ados.services.ground_station.pairing_client import (
        clear_persisted_identity,
        has_persisted_identity,
    )
    from ados.services.ground_station.pairing_manager import (
        REVOCATIONS_PATH,
    )
    from ados.services.ground_station.role_manager import (
        ROLE_FILE,
        apply_role,
    )

    mesh_wipe: dict = {"cleared_mesh": False}
    try:
        had_identity = has_persisted_identity()
        await apply_role("direct", reason="factory_reset")
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={
                "error": {
                    "code": "E_FACTORY_RESET_ROLE_FAILED",
                    "message": (
                        f"Could not downgrade to direct role before wipe: {exc}. "
                        "Pair state NOT wiped. Stop mesh services manually and retry."
                    ),
                }
            },
        ) from exc

    try:
        result = await pm.factory_reset("gs")
    except Exception as exc:
        raise HTTPException(
            status_code=500,
            detail={"error": {"code": "E_FACTORY_RESET_FAILED", "message": str(exc)}},
        ) from exc

    # Wipe mesh identity files after services are down and pair state
    # is cleared. Safe to call unconditionally; no-ops when the files
    # are already absent.
    if "error" not in mesh_wipe:
        try:
            clear_persisted_identity()
            if REVOCATIONS_PATH.is_file():
                try:
                    REVOCATIONS_PATH.unlink()
                except OSError:
                    pass
            if ROLE_FILE.is_file():
                try:
                    ROLE_FILE.unlink()
                except OSError:
                    pass
            # Gateway pin also lives under /etc/ados/mesh/. Clearing
            # it keeps "factory reset = clean slate" honest so a fresh
            # re-pair does not silently inherit the old operator's
            # preferred gateway MAC.
            gateway_path = MESH_GATEWAY_JSON
            if gateway_path.is_file():
                try:
                    gateway_path.unlink()
                except OSError:
                    pass
            mesh_wipe = {
                "cleared_mesh": had_identity,
                "role": "direct",
            }
        except Exception as exc:
            mesh_wipe = {
                "cleared_mesh": False,
                "error": str(exc),
            }

    if isinstance(result, dict):
        result.setdefault("mesh", mesh_wipe)
    else:
        result = {"result": result, "mesh": mesh_wipe}
    return result


# ---------------------------------------------------------------------------
# /display
# ---------------------------------------------------------------------------


@router.get("/display")
async def get_ground_station_display() -> dict:
    """Return the persisted HDMI kiosk display config."""
    _gs._require_ground_profile()
    return _gs._load_display_config()
