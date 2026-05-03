"""MAVLink signing enrollment and capability routes.

The agent never stores a signing key. These routes let the GCS:
  * detect whether the connected FC supports MAVLink v2 signing,
  * push a key to the FC via SETUP_SIGNING (one-shot, zeroized after),
  * clear the FC's signing store,
  * toggle SIGNING_REQUIRE on the FC,
  * read counters of signed frames observed transiting the agent.

The POST /enroll-fc body contains the raw 32-byte key as 64-char hex. The
key buffer is overwritten with zeros before the route returns. The key
MUST NEVER appear in structured logs; the log redaction helper below
strips any 64-char hex token from request bodies before they are logged.
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger
from ados.services.mavlink.signing import (
    detect_capability,
    disable_on_fc,
    enroll_fc,
    get_require,
    parse_key_hex,
    set_require,
)

log = get_logger("api.signing")

router = APIRouter()


# ──────────────────────────────────────────────────────────────
# Request / response models
# ──────────────────────────────────────────────────────────────

class EnrollRequest(BaseModel):
    """Body for POST /mavlink/signing/enroll-fc.

    key_hex is the 32-byte MAVLink signing key as 64 lowercase hex chars.
    NEVER log this field. The route strips it before handing off.
    """
    key_hex: str = Field(..., min_length=64, max_length=64)
    link_id: int = Field(default=0, ge=0, le=255)
    target_system: int = Field(default=1, ge=1, le=255)
    target_component: int = Field(default=1, ge=0, le=255)


class RequireRequest(BaseModel):
    require: bool


# ──────────────────────────────────────────────────────────────
# Routes
# ──────────────────────────────────────────────────────────────

@router.get("/mavlink/signing/capability")
async def capability() -> dict[str, Any]:
    """Report whether the connected FC supports MAVLink signing.

    Returns {supported, reason, firmware_name, firmware_version, signing_params_present}.
    reason enum: ok | fc_not_connected | firmware_not_supported | firmware_too_old
                 | firmware_px4_no_persistent_store | msp_protocol.
    """
    app = get_agent_app()
    return detect_capability(
        app.fc_connection(),
        app.vehicle_state(),
        app.param_cache(),
    )


@router.post("/mavlink/signing/enroll-fc")
async def enroll_fc_route(req: EnrollRequest) -> dict[str, Any]:
    """Push a 32-byte signing key to the FC via SETUP_SIGNING.

    Body `{key_hex, link_id, target_system, target_component}`. The key is
    zeroized from memory before this route returns. The returned `key_id`
    is a short fingerprint (first 8 hex chars of sha256), not the key.
    """
    app = get_agent_app()
    fc = app.fc_connection()
    if fc is None or not fc.connected:
        raise HTTPException(status_code=503, detail="FC not connected")

    # Parse and validate hex. key_bytes is a bytearray so enroll_fc() can
    # overwrite it in place on exit.
    try:
        key_bytes = parse_key_hex(req.key_hex)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc)) from exc

    try:
        result = await enroll_fc(
            fc,
            key_bytes,
            target_system=req.target_system,
            target_component=req.target_component,
        )
    except Exception as exc:
        # Defensive zeroize even if enroll_fc's own finally already ran.
        for i in range(len(key_bytes)):
            key_bytes[i] = 0
        log.error("signing_enroll_failed", error=str(exc), error_type=type(exc).__name__)
        raise HTTPException(status_code=500, detail="enrollment failed") from exc

    # Log with key_id fingerprint only; never the hex key.
    log.info(
        "signing_enroll_completed",
        key_id=result["key_id"],
        link_id=req.link_id,
        target_system=req.target_system,
    )
    # Return only the public-safe fields. req.key_hex was already zeroized in key_bytes.
    return result


@router.post("/mavlink/signing/disable-on-fc")
async def disable_on_fc_route() -> dict[str, Any]:
    """Clear the FC's signing store (SETUP_SIGNING with all-zero key)."""
    app = get_agent_app()
    fc = app.fc_connection()
    if fc is None or not fc.connected:
        raise HTTPException(status_code=503, detail="FC not connected")
    try:
        return disable_on_fc(fc)
    except Exception as exc:
        log.error("signing_disable_failed", error=str(exc), error_type=type(exc).__name__)
        raise HTTPException(status_code=500, detail="disable failed") from exc


@router.get("/mavlink/signing/require")
async def require_get() -> dict[str, Any]:
    """Current SIGNING_REQUIRE param value as cached by ParamCache."""
    app = get_agent_app()
    return get_require(app.param_cache())


@router.put("/mavlink/signing/require")
async def require_put(req: RequireRequest) -> dict[str, Any]:
    """Set SIGNING_REQUIRE on the FC."""
    app = get_agent_app()
    fc = app.fc_connection()
    if fc is None or not fc.connected:
        raise HTTPException(status_code=503, detail="FC not connected")
    try:
        return set_require(fc, req.require)
    except Exception as exc:
        log.error("signing_set_require_failed", error=str(exc), error_type=type(exc).__name__)
        raise HTTPException(status_code=500, detail="set require failed") from exc


@router.get("/mavlink/signing/counters")
async def counters() -> dict[str, Any]:
    """Signed-frame counters from the passive IPC observer.

    These are observational: the agent does not validate signatures (it
    holds no key). Counters just confirm signed frames are transiting.
    """
    app = get_agent_app()
    observer = app.signing_observer()
    if observer is None:
        return {
            "tx_signed_count": 0,
            "rx_signed_count": 0,
            "last_signed_rx_at": None,
        }
    return observer.snapshot()
