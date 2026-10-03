"""Peripheral Manager REST surface (``/api/v1/peripherals/*``).

A different surface from ``/api/peripherals``, the hardware scan, which
returns freshly probed USB devices, cameras, and modems for the GCS
"Sensors" panel and the cloud scan command. This v1
surface serves the plugin registry: declarative manifests from pip
packages and ``/etc/ados/peripherals/*.yaml`` plus live connection
state per manifest.

Not profile-gated. Peripherals exist on both the drone profile and
the ground-station profile, so every agent exposes this router.
"""

from __future__ import annotations

import asyncio
import shutil
from datetime import datetime, timezone
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

from ados.core.logging import get_logger
from ados.services.peripherals.registry import get_peripheral_registry

log = get_logger("api.peripherals_v1")

router = APIRouter(prefix="/v1/peripherals", tags=["peripherals"])


class PeripheralActionRequest(BaseModel):
    """Body for POST ``/v1/peripherals/{id}/action``."""

    action_id: str
    body: dict[str, Any] = Field(default_factory=dict)


def _not_found(peripheral_id: str) -> HTTPException:
    return HTTPException(
        status_code=404,
        detail={
            "code": "E_PERIPHERAL_NOT_FOUND",
            "peripheral_id": peripheral_id,
        },
    )


def _advertised(entry: dict[str, Any]) -> dict[str, Any]:
    """A registry entry with ``actions`` narrowed to the ones that dispatch.

    A manifest may declare actions nothing on this agent implements; listing
    those would offer the operator a button that cannot do anything.
    """
    peripheral_id = entry.get("id")
    actions = entry.get("actions") or []
    return {
        **entry,
        "actions": [
            a for a in actions
            if (peripheral_id, a.get("id")) in _ACTION_DISPATCHERS
        ],
    }


@router.get("")
async def list_peripherals() -> dict:
    """Return every registered peripheral manifest plus live status."""
    registry = get_peripheral_registry()
    items = [_advertised(entry) for entry in registry.list()]
    return {"peripherals": items, "count": len(items)}


@router.get("/{peripheral_id}")
async def get_peripheral(peripheral_id: str) -> dict:
    """Return a single registered peripheral manifest plus live status.

    Returns 404 if the id is not registered.
    """
    registry = get_peripheral_registry()
    entry = registry.get(peripheral_id)
    if entry is None:
        raise _not_found(peripheral_id)
    return _advertised(entry)


@router.post("/{peripheral_id}/config")
async def put_peripheral_config(
    peripheral_id: str,
    body: dict[str, Any],
) -> dict:
    """Refuse a config write: nothing on this agent consumes peripheral config.

    Persisting the blob and answering success would tell the operator a
    setting took effect when no process ever reads it.
    """
    if get_peripheral_registry().get_manifest(peripheral_id) is None:
        raise _not_found(peripheral_id)
    raise HTTPException(
        status_code=501,
        detail={
            "error": {
                "code": "E_NOT_SUPPORTED",
                "message": "peripheral configuration is not supported on this agent",
                "peripheral_id": peripheral_id,
            }
        },
    )


async def _systemctl(*args: str, timeout: float) -> tuple[int, str, str]:
    """Run ``systemctl`` off the event loop, bounded, killing it on timeout."""
    proc = await asyncio.create_subprocess_exec(
        "systemctl",
        *args,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        out, err = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise
    rc = proc.returncode if proc.returncode is not None else -1
    return rc, out.decode(errors="replace"), err.decode(errors="replace")


def _wfb_unit() -> str:
    """The radio unit this node actually runs: ``ados-wfb-rx`` on a ground
    station (where ``ados-wfb`` is a no-op), ``ados-wfb`` everywhere else."""
    from ados.api.deps import get_agent_app
    from ados.core.profile import current_profile_and_role

    profile, _ = current_profile_and_role(get_agent_app().config)
    return "ados-wfb-rx" if profile == "ground-station" else "ados-wfb"


async def _dispatch_restart_radio() -> dict:
    """Restart the WFB radio service via systemctl.

    Pre-flight ``systemctl is-active <unit>`` so a profile that doesn't run
    the unit (drone-only without an RTL8812EU adapter) surfaces a clean 409
    instead of pretending to succeed. The unit follows the node's profile:
    a ground station restarts ``ados-wfb-rx``. Both systemctl calls run as
    subprocesses awaited off the event loop, so the rest of the API keeps
    serving while the radio restarts.
    """
    if shutil.which("systemctl") is None:
        raise HTTPException(
            status_code=409,
            detail={
                "code": "E_SYSTEMCTL_MISSING",
                "message": "systemctl unavailable on this host",
            },
        )
    unit = _wfb_unit()
    try:
        _rc, stdout, _err = await _systemctl("is-active", unit, timeout=5)
    except (TimeoutError, OSError) as exc:
        raise HTTPException(
            status_code=500,
            detail={"code": "E_PROBE_FAILED", "message": str(exc) or "probe timed out"},
        ) from exc
    state = stdout.strip()
    # Whitelist only the stable "active" state. activating /
    # deactivating / reloading all indicate the supervisor is mid-
    # transition; restarting on top of a deactivate path races the
    # operator's intent. The 409 envelope tells the caller why.
    if state != "active":
        raise HTTPException(
            status_code=409,
            detail={
                "code": "E_UNIT_NOT_RUNNING",
                "unit": unit,
                "state": state or "unknown",
                "message": (
                    "The wfb radio service is not in the stable active "
                    "state. Restart cannot proceed."
                ),
            },
        )
    try:
        rc, _out, stderr = await _systemctl("restart", unit, timeout=15)
    except TimeoutError as exc:
        raise HTTPException(
            status_code=504,
            detail={
                "code": "E_RESTART_TIMEOUT",
                "message": f"systemctl restart {unit} timed out",
            },
        ) from exc
    except OSError as exc:
        raise HTTPException(
            status_code=500,
            detail={"code": "E_RESTART_FAILED", "message": str(exc)},
        ) from exc
    if rc != 0:
        raise HTTPException(
            status_code=500,
            detail={
                "code": "E_RESTART_FAILED",
                "message": stderr.strip() or f"systemctl exited {rc}",
            },
        )
    return {
        "ok": True,
        "dispatched_at": datetime.now(timezone.utc).isoformat(),
        "message": f"{unit} restarted",
    }


# Map of (peripheral_id, action_id) -> dispatcher. Returning a dict is
# the wire shape the dashboard renders. Raise HTTPException for clean
# 4xx / 5xx responses. An action not in the map is refused with a 501 and
# is left out of the listing, so no surface offers it.
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
    a declared action with no dispatcher is a 501
    ``E_ACTION_NOT_SUPPORTED`` rather than a reported success.
    """
    registry = get_peripheral_registry()
    manifest = registry.get_manifest(peripheral_id)
    if manifest is None:
        raise _not_found(peripheral_id)

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
        raise HTTPException(
            status_code=501,
            detail={
                "error": {
                    "code": "E_ACTION_NOT_SUPPORTED",
                    "message": "this action has no implementation on this agent",
                    "peripheral_id": peripheral_id,
                    "action_id": request.action_id,
                }
            },
        )

    result = await dispatcher()
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
