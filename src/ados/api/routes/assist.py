"""Assist service REST routes.

All endpoints are under /api/assist/*.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

import structlog

log = structlog.get_logger()
router = APIRouter(prefix="/assist", tags=["assist"])


def _svc():
    try:
        from ados.api.deps import get_agent_app
        app = get_agent_app()
        return getattr(app, "_assist_service", None)
    except Exception:
        return None


@router.get("/status")
async def status():
    svc = _svc()
    if not svc:
        return {"enabled": False, "service": "unavailable"}
    return {**svc.get_status(), "service": "running"}


@router.get("/suggestions")
async def list_suggestions():
    svc = _svc()
    if not svc:
        return []
    return svc.get_suggestions()


@router.post("/suggestions/{suggestion_id}/acknowledge")
async def acknowledge_suggestion(suggestion_id: str):
    svc = _svc()
    if not svc or not svc._emitter:
        raise HTTPException(status_code=503, detail="Assist service unavailable")
    ok = svc._emitter.acknowledge(suggestion_id)
    if not ok:
        raise HTTPException(status_code=404, detail="Suggestion not found")
    return {"ok": True}


@router.post("/suggestions/{suggestion_id}/dismiss")
async def dismiss_suggestion(suggestion_id: str):
    svc = _svc()
    if not svc or not svc._emitter:
        raise HTTPException(status_code=503, detail="Assist service unavailable")
    ok = svc._emitter.dismiss(suggestion_id)
    if not ok:
        raise HTTPException(status_code=404, detail="Suggestion not found")
    return {"ok": True}


@router.get("/repairs")
async def list_repairs():
    svc = _svc()
    if not svc:
        return []
    return svc.get_repairs()


class ApproveBody(BaseModel):
    pass


@router.post("/repairs/{repair_id}/approve")
async def approve_repair(repair_id: str):
    svc = _svc()
    if not svc:
        raise HTTPException(status_code=503, detail="Assist service unavailable")
    item = svc.repair_queue.approve(repair_id)
    if not item:
        raise HTTPException(status_code=404, detail="Repair item not found or not in pending state")
    return {"ok": True, "item": item.to_dict()}


@router.post("/repairs/{repair_id}/reject")
async def reject_repair(repair_id: str):
    svc = _svc()
    if not svc:
        raise HTTPException(status_code=503, detail="Assist service unavailable")
    ok = svc.repair_queue.reject(repair_id)
    if not ok:
        raise HTTPException(status_code=404, detail="Repair item not found")
    return {"ok": True}


@router.post("/repairs/{repair_id}/rollback")
async def rollback_repair(repair_id: str):
    svc = _svc()
    if not svc:
        raise HTTPException(status_code=503, detail="Assist service unavailable")
    ok = svc.repair_queue.rollback(repair_id)
    if not ok:
        raise HTTPException(status_code=404, detail="Repair item not found or not applied")
    return {"ok": True}


@router.get("/diagnostics/snapshot")
async def get_diagnostic_snapshot():
    svc = _svc()
    if not svc:
        return {"available": False}
    return {**svc.ctx.snapshot(), "available": True}
