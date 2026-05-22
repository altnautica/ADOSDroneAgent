"""Script management and text command API routes."""

from __future__ import annotations

from dataclasses import asdict

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger
from ados.services.scripting import script_library

log = get_logger("api.scripts")

router = APIRouter()


class RunScriptRequest(BaseModel):
    path: str


class StopScriptRequest(BaseModel):
    script_id: str


class TextCommandRequest(BaseModel):
    command: str


@router.get("/scripts")
async def list_scripts():
    """List saved scripts, recent executions, and the command log.

    The ``scripts`` field carries the persistent library — the rows
    the Mission Control Scripts tab and the agent webapp render and
    edit. ``executions`` carries the per-run history (state, pid,
    output tail) so an operator can still inspect what fired off
    most recently. ``command_log`` is the text-command audit trail.
    """
    app = get_agent_app()
    handles = app.scripting_handles()
    runner = handles.runner
    executor = handles.executor
    demo = handles.demo

    # Saved scripts live on disk under SCRIPTS_DIR and are readable
    # from every process — the API process serves the route here,
    # the supervisor process owns the runner. Pull the library
    # straight from the filesystem module rather than reaching for
    # the runner so the route works in either process layout.
    scripts: list[dict] = [asdict(s) for s in script_library.list_saved_scripts()]
    executions: list[dict] = []
    command_log: list[dict] = []

    if runner is not None:
        for info in runner.list_scripts():
            executions.append({
                "script_id": info.script_id,
                "filename": info.filename,
                "state": info.state.value,
                "pid": info.pid,
                "started_at": info.started_at,
                "finished_at": info.finished_at,
                "output_lines": info.output_lines[-20:],
                "return_code": info.return_code,
            })

    if executor is not None:
        command_log = [
            {
                "timestamp": e.timestamp,
                "command": e.command,
                "source": e.source,
                "result": e.result,
            }
            for e in executor.command_log[-50:]
        ]
    elif demo is not None:
        command_log = demo.command_log[-50:]

    return {
        "scripts": scripts,
        "executions": executions,
        "command_log": command_log,
    }


class SaveScriptRequest(BaseModel):
    name: str
    content: str
    suite: str | None = None


@router.post("/scripts")
async def save_script(req: SaveScriptRequest):
    """Persist a script to disk and return its server-assigned id.

    The library is a disk-backed shared resource, so the API process
    handles the write directly. The supervisor process (which owns
    the runner) reads the same files when an operator launches a
    script.
    """
    try:
        saved = script_library.save_script(req.name, req.content, req.suite)
    except RuntimeError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    return asdict(saved)


@router.delete("/scripts/{script_id}")
async def delete_script(script_id: str):
    """Remove a saved script by id. 404 when the id is unknown."""
    removed = script_library.delete_script(script_id)
    if not removed:
        raise HTTPException(status_code=404, detail="Saved script not found")
    return {"status": "ok", "script_id": script_id}


@router.post("/scripts/{script_id}/run")
async def run_script_by_id(script_id: str):
    """Run a saved script by its server-assigned id. The runner
    materialises the persisted source to a temp file and queues it
    through the regular execution path."""
    app = get_agent_app()
    runner = app.scripting_handles().runner
    if runner is None:
        raise HTTPException(status_code=503, detail="Script runner not available")
    try:
        sid = runner.start_saved_script(script_id)
    except RuntimeError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    return {"status": "ok", "script_id": sid}


@router.post("/scripts/run")
async def run_script(req: RunScriptRequest):
    """Start a Python script."""
    app = get_agent_app()
    runner = app.scripting_handles().runner
    if runner is None:
        raise HTTPException(status_code=503, detail="Script runner not available")

    try:
        script_id = runner.start_script(req.path)
    except RuntimeError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    return {"status": "ok", "script_id": script_id}


@router.post("/scripts/stop")
async def stop_script(req: StopScriptRequest):
    """Stop a running script."""
    app = get_agent_app()
    runner = app.scripting_handles().runner
    if runner is None:
        raise HTTPException(status_code=503, detail="Script runner not available")

    stopped = runner.stop_script(req.script_id)
    if not stopped:
        raise HTTPException(status_code=404, detail="Script not found or not running")
    return {"status": "ok", "script_id": req.script_id}


@router.post("/scripting/command")
async def execute_text_command(req: TextCommandRequest):
    """Execute a single text command through the scripting engine."""
    app = get_agent_app()

    from ados.services.scripting.text_parser import parse_text_command
    parsed = parse_text_command(req.command)

    # Try demo engine first, then real executor
    handles = app.scripting_handles()
    demo = handles.demo
    executor = handles.executor

    if demo is not None:
        result = await demo.execute(parsed, source="api")
        return {"status": "ok" if not result.startswith("error") else "error", "result": result}

    if executor is not None:
        result = await executor.execute(parsed, source="api")
        return {"status": "ok" if not result.startswith("error") else "error", "result": result}

    raise HTTPException(status_code=503, detail="Scripting engine not available")


@router.get("/scripting/status")
async def scripting_status():
    """Overall scripting engine status."""
    app = get_agent_app()

    handles = app.scripting_handles()
    demo = handles.demo
    if demo is not None:
        return demo.status()

    executor = handles.executor
    runner = handles.runner

    result: dict = {"demo_mode": False}

    if executor is not None:
        result["commands_executed"] = len(executor.command_log)

    if runner is not None:
        scripts = runner.list_scripts()
        result["scripts_total"] = len(scripts)
        result["scripts_running"] = sum(
            1 for s in scripts if s.state.value == "running"
        )

    fc = app.fc_connection()
    result["fc_connected"] = bool(fc and getattr(fc, "connected", False))

    return result
