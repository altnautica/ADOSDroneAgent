"""Rerun sink REST routes.

Routes query the ados-rerun-sink.service via its local state file
plus direct filesystem inspection of the recordings directory.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

from fastapi import APIRouter
from pydantic import BaseModel

router = APIRouter(prefix="/rerun", tags=["rerun"])

STATUS_FILE = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "rerun_status.json"
RECORDINGS_DIR = Path(os.environ.get("ADOS_RERUN_DIR", "/var/ados/rerun"))


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
    for f in sorted(RECORDINGS_DIR.glob("*.rrd"), key=lambda p: p.stat().st_mtime, reverse=True):
        out.append({
            "name": f.name,
            "path": str(f),
            "size_bytes": f.stat().st_size,
            "created_at": f.stat().st_mtime,
            "download_url": f"/api/rerun/recordings/{f.name}/download",
        })
    return out


@router.get("/status")
async def status():
    st = _read_status()
    return {
        "status": "running" if st else "unavailable",
        "port": 9876,
        "recording": bool(st and st.get("recording", False)),
    }


class RecordStartBody(BaseModel):
    filename: str = ""


@router.post("/record/start")
async def start_recording(body: RecordStartBody):
    cmd_file = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "rerun_cmd.json"
    try:
        cmd_file.write_text(json.dumps({"action": "start", "filename": body.filename}))
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


@router.post("/record/stop")
async def stop_recording():
    cmd_file = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "rerun_cmd.json"
    try:
        cmd_file.write_text(json.dumps({"action": "stop"}))
        return {"ok": True}
    except OSError as e:
        return {"ok": False, "error": str(e)}


@router.get("/recordings")
async def list_recordings():
    return _list_recordings()
