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

import json
import os
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field

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


def _atomic_write_json(path: Path, payload: dict) -> None:
    """Write JSON to ``path`` via a sibling temp file + rename."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    fd = os.open(str(tmp), os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
    try:
        os.write(fd, json.dumps(payload, indent=2, sort_keys=True).encode("utf-8"))
        os.fsync(fd)
    finally:
        os.close(fd)
    os.chmod(tmp, 0o644)
    os.rename(tmp, path)


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
        _atomic_write_json(path, body)
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


@router.post("/{peripheral_id}/action")
async def invoke_peripheral_action(
    peripheral_id: str,
    request: PeripheralActionRequest,
) -> dict:
    """Queue an action against the peripheral.

    Wave 3 validates that the action is declared on the manifest and
    returns a stubbed ``{queued: True}`` response. Real plugin
    dispatch lands with Track B.
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

    log.info(
        "peripheral_action_queued",
        peripheral_id=peripheral_id,
        action_id=request.action_id,
    )
    return {
        "queued": True,
        "peripheral_id": peripheral_id,
        "action_id": request.action_id,
    }
