"""Assist service REST routes.

All endpoints are under /api/assist/*.

Routes read state from files written by ados-assist.service:
  /run/ados/assist_status.json        - current status (collector_count, counts)
  /var/ados/assist/suggestions.json   - active suggestions list
  /var/ados/assist/repairs.json       - repair queue

Command channels (routes POST here, service polls):
  /run/ados/assist_cmd.json           - acknowledge/dismiss/approve/reject
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

import structlog

log = structlog.get_logger()
router = APIRouter(prefix="/assist", tags=["assist"])

RUN_DIR = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados"))
STATE_DIR = Path(os.environ.get("ADOS_STATE_DIR", "/var/ados/assist"))

STATUS_FILE = RUN_DIR / "assist_status.json"
SUGGESTIONS_FILE = STATE_DIR / "suggestions.json"
REPAIRS_FILE = STATE_DIR / "repairs.json"
CMD_FILE = RUN_DIR / "assist_cmd.json"


def _read_json(path: Path, default):
    if not path.exists():
        return default
    try:
        return json.loads(path.read_text())
    except Exception:
        return default


def _write_cmd(cmd: dict) -> None:
    CMD_FILE.parent.mkdir(parents=True, exist_ok=True)
    CMD_FILE.write_text(json.dumps({**cmd, "ts": time.time()}))


@router.get("/status")
async def status():
    """Return Assist service status. Falls back to 'unavailable' if service offline."""
    status = _read_json(STATUS_FILE, None)
    if status is None:
        return {
            "enabled": False,
            "service": "unavailable",
            "active_suggestions": 0,
            "pending_repairs": 0,
            "collector_count": 0,
        }
    # Freshness check — if status is older than 30s, assume service stopped.
    if time.time() - status.get("ts", 0) > 30:
        return {"enabled": False, "service": "stale", "active_suggestions": 0, "pending_repairs": 0}
    return {**status, "service": "running"}


@router.get("/suggestions")
async def list_suggestions():
    return _read_json(SUGGESTIONS_FILE, [])


@router.post("/suggestions/{suggestion_id}/acknowledge")
async def acknowledge_suggestion(suggestion_id: str):
    _write_cmd({"action": "acknowledge", "suggestion_id": suggestion_id})
    return {"ok": True}


@router.post("/suggestions/{suggestion_id}/dismiss")
async def dismiss_suggestion(suggestion_id: str):
    _write_cmd({"action": "dismiss", "suggestion_id": suggestion_id})
    return {"ok": True}


@router.get("/repairs")
async def list_repairs():
    return _read_json(REPAIRS_FILE, [])


@router.post("/repairs/{repair_id}/approve")
async def approve_repair(repair_id: str):
    _write_cmd({"action": "approve_repair", "repair_id": repair_id})
    return {"ok": True}


@router.post("/repairs/{repair_id}/reject")
async def reject_repair(repair_id: str):
    _write_cmd({"action": "reject_repair", "repair_id": repair_id})
    return {"ok": True}


@router.post("/repairs/{repair_id}/rollback")
async def rollback_repair(repair_id: str):
    _write_cmd({"action": "rollback_repair", "repair_id": repair_id})
    return {"ok": True}


@router.get("/diagnostics/snapshot")
async def get_diagnostic_snapshot():
    snapshot_file = STATE_DIR / "snapshot.json"
    snap = _read_json(snapshot_file, None)
    if snap is None:
        return {"available": False}
    return {**snap, "available": True}
