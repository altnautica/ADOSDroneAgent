"""Peripheral Manager REST surface (``/api/v1/peripherals/*``).

Lives alongside the legacy ``/api/peripherals`` hardware scan route
rather than replacing it. The legacy route returns freshly probed USB
devices, cameras, and modems for the GCS "Sensors" panel. This v1
surface serves the plugin registry: declarative manifests from pip
packages and ``/etc/ados/peripherals/*.yaml`` plus live connection
state per manifest.

Not profile-gated. Peripherals exist on both the drone profile and
the ground-station profile, so every agent exposes this router.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.core.atomic import atomic_write_json
from ados.core.logging import get_logger
from ados.core.paths import PERIPHERALS_DIR
from ados.services.peripherals.registry import get_peripheral_registry

log = get_logger("api.peripherals_v1")

router = APIRouter(prefix="/v1/peripherals", tags=["peripherals"])

_CONFIG_DIR = PERIPHERALS_DIR


class PeripheralActionRequest(BaseModel):
    """Body for POST ``/v1/peripherals/{id}/action``."""

    action_id: str
    body: dict[str, Any] = Field(default_factory=dict)


def _config_path(peripheral_id: str) -> Path:
    """Return the persisted-config path for a given peripheral id.

    Sanitizes the id so path traversal is impossible. The registry
    already owns the canonical id; this guard is defense in depth.
    """
    safe = peripheral_id.replace("/", "_").replace("..", "_")
    return _CONFIG_DIR / f"{safe}.config.json"


@router.get("")
async def list_peripherals() -> dict:
    """Return every registered peripheral manifest plus live status."""
    registry = get_peripheral_registry()
    items = registry.list()
    return {"peripherals": items, "count": len(items)}


@router.get("/{peripheral_id}")
async def get_peripheral(peripheral_id: str) -> dict:
    """Return a single registered peripheral manifest plus live status.

    Returns 404 if the id is not registered.
    """
    registry = get_peripheral_registry()
    entry = registry.get(peripheral_id)
    if entry is None:
        raise HTTPException(
            status_code=404,
            detail={
                "code": "E_PERIPHERAL_NOT_FOUND",
                "peripheral_id": peripheral_id,
            },
        )
    return entry


@router.post("/{peripheral_id}/config")
async def put_peripheral_config(
    peripheral_id: str,
    body: dict[str, Any],
) -> dict:
    """Persist a config blob for the given peripheral.

    Wave 3 behavior: validate against the manifest's ``config_schema``
    if one is declared, then write ``/etc/ados/peripherals/<id>.config.json``
    atomically. Plugin-side consumption of this file arrives with
    Track B.
    """
    registry = get_peripheral_registry()
    manifest = registry.get_manifest(peripheral_id)
    if manifest is None:
        raise HTTPException(
            status_code=404,
            detail={
                "code": "E_PERIPHERAL_NOT_FOUND",
                "peripheral_id": peripheral_id,
            },
        )

    if manifest.config_schema:
        try:
            # Lazy import so jsonschema stays optional. If the library
            # is not present, log the gap and let the write through.
            # Strict validation is plugin responsibility in Track B.
            import jsonschema  # type: ignore[import-not-found]
            jsonschema.validate(instance=body, schema=manifest.config_schema)
        except ImportError:
            log.debug(
                "peripheral_config_validate_skipped",
                peripheral_id=peripheral_id,
                reason="jsonschema_not_installed",
            )
        except jsonschema.ValidationError as exc:  # type: ignore[attr-defined]
            raise HTTPException(
                status_code=400,
                detail={
                    "code": "E_CONFIG_SCHEMA_INVALID",
                    "peripheral_id": peripheral_id,
                    "message": exc.message,
                    "path": list(exc.absolute_path),
                },
            ) from exc

    path = _config_path(peripheral_id)
    try:
        atomic_write_json(path, body, mode=0o644, sort_keys=True)
    except OSError as exc:
        log.error(
            "peripheral_config_write_failed",
            peripheral_id=peripheral_id,
            path=str(path),
            error=str(exc),
        )
        raise HTTPException(
            status_code=500,
            detail={
                "code": "E_CONFIG_WRITE_FAILED",
                "peripheral_id": peripheral_id,
            },
        ) from exc

    log.info(
        "peripheral_config_written",
        peripheral_id=peripheral_id,
        path=str(path),
    )
    return {
        "persisted": True,
        "peripheral_id": peripheral_id,
        "path": str(path),
    }


def _dispatch_restart_radio() -> dict:
    """Restart the WFB radio service via systemctl.

    Pre-flight ``systemctl is-active ados-wfb`` so a profile that
    doesn't run the unit (drone-only without an RTL8812EU adapter)
    surfaces a clean 409 instead of pretending to succeed. The
    ``ados-wfb`` unit is on the same allowlist used by the dedicated
    service-restart route in ``services.py``.
    """
    import shutil
    import subprocess
    from datetime import datetime, timezone

    if shutil.which("systemctl") is None:
        raise HTTPException(
            status_code=409,
            detail={
                "code": "E_SYSTEMCTL_MISSING",
                "message": "systemctl unavailable on this host",
            },
        )
    try:
        active = subprocess.run(
            ["systemctl", "is-active", "ados-wfb"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (subprocess.SubprocessError, FileNotFoundError) as exc:
        raise HTTPException(
            status_code=500,
            detail={"code": "E_PROBE_FAILED", "message": str(exc)},
        ) from exc
    state = (active.stdout or "").strip()
    # Whitelist only the stable "active" state. activating /
    # deactivating / reloading all indicate the supervisor is mid-
    # transition; restarting on top of a deactivate path races the
    # operator's intent. The 409 envelope tells the caller why.
    if state != "active":
        raise HTTPException(
            status_code=409,
            detail={
                "code": "E_UNIT_NOT_RUNNING",
                "unit": "ados-wfb",
                "state": state or "unknown",
                "message": (
                    "The wfb radio service is not in the stable active "
                    "state. Restart cannot proceed."
                ),
            },
        )
    try:
        result = subprocess.run(
            ["systemctl", "restart", "ados-wfb"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except subprocess.TimeoutExpired as exc:
        raise HTTPException(
            status_code=504,
            detail={
                "code": "E_RESTART_TIMEOUT",
                "message": "systemctl restart ados-wfb timed out",
            },
        ) from exc
    if result.returncode != 0:
        raise HTTPException(
            status_code=500,
            detail={
                "code": "E_RESTART_FAILED",
                "message": (result.stderr or "").strip()
                or f"systemctl exited {result.returncode}",
            },
        )
    return {
        "ok": True,
        "dispatched_at": datetime.now(timezone.utc).isoformat(),
        "message": "ados-wfb restarted",
    }


# Map of (peripheral_id, action_id) -> dispatcher. Returning a dict is
# the wire shape the dashboard renders. Raise HTTPException for clean
# 4xx / 5xx responses. Anything not in the map falls through to the
# generic "action declared but not wired yet" stub so the dashboard can
# still surface the button without lying about completion.
_ACTION_DISPATCHERS = {
    ("ados.rtl8812eu-radio", "restart_radio"): _dispatch_restart_radio,
}


@router.post("/{peripheral_id}/action")
async def invoke_peripheral_action(
    peripheral_id: str,
    request: PeripheralActionRequest,
) -> dict:
    """Invoke an action against the peripheral.

    Validates the action is declared on the manifest, then looks up a
    real dispatcher in the ``_ACTION_DISPATCHERS`` table. Wired
    actions execute and return ``{ok: true, dispatched_at, message?}``;
    declared-but-not-yet-wired actions return a clear
    ``{queued: false, reason}`` envelope so the dashboard surfaces a
    "not yet implemented" message rather than pretending success.
    """
    registry = get_peripheral_registry()
    manifest = registry.get_manifest(peripheral_id)
    if manifest is None:
        raise HTTPException(
            status_code=404,
            detail={
                "code": "E_PERIPHERAL_NOT_FOUND",
                "peripheral_id": peripheral_id,
            },
        )

    declared = {action.id for action in manifest.actions}
    if request.action_id not in declared:
        raise HTTPException(
            status_code=400,
            detail={
                "code": "E_ACTION_NOT_DECLARED",
                "peripheral_id": peripheral_id,
                "action_id": request.action_id,
                "declared_actions": sorted(declared),
            },
        )

    dispatcher = _ACTION_DISPATCHERS.get((peripheral_id, request.action_id))
    if dispatcher is None:
        log.info(
            "peripheral_action_not_wired",
            peripheral_id=peripheral_id,
            action_id=request.action_id,
        )
        return {
            "ok": False,
            "peripheral_id": peripheral_id,
            "action_id": request.action_id,
            "reason": "action_declared_but_not_yet_wired",
            "message": (
                "The agent recognises this action but no dispatcher is "
                "wired yet. The manifest entry is the contract; "
                "implementation lands separately."
            ),
        }

    result = dispatcher()
    log.info(
        "peripheral_action_dispatched",
        peripheral_id=peripheral_id,
        action_id=request.action_id,
    )
    return {
        "peripheral_id": peripheral_id,
        "action_id": request.action_id,
        **result,
    }
