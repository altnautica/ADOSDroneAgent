"""Rerun sink REST routes."""

from __future__ import annotations

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter(prefix="/rerun", tags=["rerun"])


def _svc():
    try:
        from ados.api.deps import get_agent_app
        app = get_agent_app()
        return getattr(app, "_rerun_service", None)
    except Exception:
        return None


@router.get("/status")
async def status():
    svc = _svc()
    return {
        "status": "running" if svc else "unavailable",
        "port": 9876,
        "recording": svc.recording if svc else False,
    }


class RecordStartBody(BaseModel):
    filename: str = ""


@router.post("/record/start")
async def start_recording(body: RecordStartBody):
    svc = _svc()
    if not svc:
        return {"error": "service unavailable"}
    path = svc.start_recording(body.filename)
    return {"ok": True, "path": path}


@router.post("/record/stop")
async def stop_recording():
    svc = _svc()
    if not svc:
        return {"error": "service unavailable"}
    path = svc.stop_recording()
    return {"ok": True, "path": path}


@router.get("/recordings")
async def list_recordings():
    svc = _svc()
    if not svc:
        return []
    return svc.list_recordings()
