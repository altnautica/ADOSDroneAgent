"""Foxglove bridge REST routes.

Routes query the ados-foxglove-bridge.service via its local state file
plus direct filesystem inspection of the recordings directory.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter(prefix="/foxglove", tags=["foxglove"])

STATUS_FILE = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "foxglove_status.json"
RECORDINGS_DIR = Path(os.environ.get("ADOS_FOXGLOVE_DIR", "/var/ados/foxglove"))


def _read_status() -> dict | None:
    if not STATUS_FILE.exists():
        return None
    try:
        import time
        data = json.loads(STATUS_FILE.read_text())
        if time.time() - data.get("ts", 0) > 30:
            return None
        return data
    except Exception:
        return None


def _list_recordings() -> list[dict]:
    if not RECORDINGS_DIR.exists():
        return []
    out = []
    for f in sorted(RECORDINGS_DIR.glob("*.mcap"), key=lambda p: p.stat().st_mtime, reverse=True):
        out.append({
            "name": f.name,
            "path": str(f),
            "size_bytes": f.stat().st_size,
            "created_at": f.stat().st_mtime,
        })
    return out


@router.get("/status")
async def status():
    st = _read_status()
    return {
        "status": "running" if st else "unavailable",
        "port": 8765,
        "recording": bool(st and st.get("recording", False)),
        "recording_path": st.get("recording_path") if st else None,
    }


class RecordBody(BaseModel):
    filename: str = ""


@router.post("/record")
async def start_recording(body: RecordBody):
    """Trigger the bridge service to start MCAP recording.

    Writes a command file that the bridge service polls every second.
    """
    cmd_file = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "foxglove_cmd.json"
    try:
        cmd_file.write_text(json.dumps({"action": "start", "filename": body.filename}))
        return {"ok": True, "note": "Recording will start within 1 second"}
    except OSError as e:
        return {"ok": False, "error": str(e)}


@router.delete("/record")
async def stop_recording():
    cmd_file = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "foxglove_cmd.json"
    try:
        cmd_file.write_text(json.dumps({"action": "stop"}))
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


@router.get("/recordings")
async def list_recordings():
    return _list_recordings()
