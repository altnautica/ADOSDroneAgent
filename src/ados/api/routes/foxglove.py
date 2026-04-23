"""Foxglove bridge REST routes."""

from __future__ import annotations

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter(prefix="/foxglove", tags=["foxglove"])


def _svc():
    try:
        from ados.api.deps import get_agent_app
        app = get_agent_app()
        return getattr(app, "_foxglove_service", None)
    except Exception:
        return None


@router.get("/status")
async def status():
    svc = _svc()
    return {
        "status": "running" if svc else "unavailable",
        "port": 8765,
        "recording": svc.recording if svc else False,
        "recording_path": svc.recording_path if svc else None,
    }


class RecordBody(BaseModel):
    filename: str = ""


@router.post("/record")
async def start_recording(body: RecordBody):
    svc = _svc()
    if not svc:
        return {"error": "service unavailable"}
    path = svc.start_recording(body.filename)
    return {"ok": True, "path": path}


@router.delete("/record")
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
