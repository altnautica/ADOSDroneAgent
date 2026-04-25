"""ROS 2 environment management API routes.

REST endpoints for initializing, monitoring, and controlling the
opt-in ROS 2 Jazzy Docker environment.

Live endpoints: /api/ros/status, /api/ros/init, /api/ros/nodes, /api/ros/topics
Stub endpoints: launch, stop, workspace, build, recording, tunnel
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from fastapi import APIRouter, HTTPException
from fastapi.responses import StreamingResponse
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.services.ros_manager import RosManager, RosState, get_ros_manager

log = logging.getLogger("ados.api.routes.ros")

router = APIRouter(tags=["ros"])


# ── Request / Response models ────────────────────────────────────────

class RosInitRequest(BaseModel):
    profile: str = "minimal"
    middleware: str = "zenoh"
    delivery_mode: str = "online"  # online | offline


class RosStatusResponse(BaseModel):
    state: str
    error: str | None = None
    distro: str = "jazzy"
    middleware: str = "zenoh"
    profile: str = "minimal"
    foxglove_port: int = 8766
    foxglove_url: str | None = None
    container_id: str | None = None
    uptime_s: int | None = None
    nodes_count: int = 0
    topics_count: int = 0


class RosNodeInfo(BaseModel):
    name: str
    package: str = ""
    pid: int | None = None
    publishes: list[str] = Field(default_factory=list)
    subscribes: list[str] = Field(default_factory=list)


class RosTopicInfo(BaseModel):
    name: str
    type: str = ""
    publishers: int = 0
    subscribers: int = 0
    rate_hz: float | None = None


# ── Helpers ──────────────────────────────────────────────────────────

def _get_manager() -> RosManager:
    """Get the ROS manager, checking board support first."""
    app = get_agent_app()

    # Check board profile for ROS support
    if hasattr(app, "board_profile") and app.board_profile:
        ros_cfg = app.board_profile.get("ros", {})
        if not ros_cfg.get("supported", False):
            raise HTTPException(
                status_code=412,
                detail="This board does not support ROS 2 (ros.supported=false in board profile)",
            )

    return get_ros_manager(app.config)


# ── Routes ───────────────────────────────────────────────────────────

@router.get("/ros/status")
async def get_ros_status() -> RosStatusResponse:
    """Get current ROS environment status."""
    try:
        manager = _get_manager()
    except HTTPException:
        return RosStatusResponse(
            state="not_supported",
            error="Board does not support ROS 2",
        )

    status = manager.get_status()
    return RosStatusResponse(**status)


@router.post("/ros/init")
async def initialize_ros(req: RosInitRequest) -> StreamingResponse:
    """Initialize the ROS 2 environment.

    Returns an SSE stream with progress events.
    """
    manager = _get_manager()

    if manager.state == RosState.RUNNING:
        raise HTTPException(status_code=409, detail="ROS environment is already running")
    if manager.state == RosState.INITIALIZING:
        raise HTTPException(status_code=409, detail="ROS initialization already in progress")

    # Check prerequisites
    issues = manager.check_prerequisites()
    if issues:
        raise HTTPException(
            status_code=412,
            detail={"message": "Prerequisites not met", "issues": issues},
        )

    async def event_stream():
        """SSE progress stream. Cancels init_task on client disconnect."""
        init_task: asyncio.Task | None = None
        try:
            yield _sse_event("step", {"step": "preflight", "message": "Checking prerequisites..."})

            yield _sse_event("step", {"step": "image", "message": "Preparing Docker image..."})

            # Run initialization in background
            init_task = asyncio.create_task(manager.initialize())

            # Poll for state changes while initializing
            prev_state = None
            while not init_task.done():
                current = manager.state.value
                if current != prev_state:
                    yield _sse_event("progress", {"state": current})
                    prev_state = current
                await asyncio.sleep(1)

            success = await init_task
            if success:
                yield _sse_event("done", {"state": "running", "message": "ROS environment ready"})
            else:
                yield _sse_event("error", {"state": "error", "message": manager.error or "Unknown error"})
        except (GeneratorExit, asyncio.CancelledError):
            # Client disconnected. Cancel the background init task.
            if init_task and not init_task.done():
                init_task.cancel()
                log.info("SSE client disconnected, cancelled ROS init task")
            raise

    return StreamingResponse(
        event_stream(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive"},
    )


@router.get("/ros/nodes")
async def get_ros_nodes() -> list[RosNodeInfo]:
    """List running ROS 2 nodes with publisher/subscriber info."""
    manager = _get_manager()
    if manager.state != RosState.RUNNING:
        return []

    nodes = manager.get_nodes()
    return [RosNodeInfo(**n) for n in nodes]


@router.get("/ros/topics")
async def get_ros_topics() -> list[RosTopicInfo]:
    """List active ROS 2 topics with types and rates."""
    manager = _get_manager()
    if manager.state != RosState.RUNNING:
        return []

    topics = manager.get_topics()
    return [RosTopicInfo(**t) for t in topics]


@router.post("/ros/stop")
async def stop_ros() -> dict[str, str]:
    """Stop the ROS 2 environment."""
    manager = _get_manager()
    await manager.stop()
    return {"status": "stopped"}


# ── Workspace, Launch, Recording ────────────────────────────────────

@router.post("/ros/launch")
async def launch_node(package: str = "", executable: str = "", name: str = "") -> dict:
    """Launch a user ROS node inside the container."""
    manager = _get_manager()
    if manager.state != RosState.RUNNING:
        raise HTTPException(status_code=412, detail="ROS environment is not running")
    if not package:
        raise HTTPException(status_code=422, detail="package is required")

    # Build ros2 run command
    cmd_parts = ["run", package]
    if executable:
        cmd_parts.append(executable)

    result = manager._exec_ros2_cmd(*cmd_parts, timeout=15)
    if result is None:
        raise HTTPException(status_code=500, detail="Failed to launch node")

    return {"status": "launched", "package": package, "executable": executable}


@router.get("/ros/workspace")
async def get_workspace() -> dict:
    """Get workspace metadata: packages, last build, disk usage."""
    manager = _get_manager()
    if manager.state != RosState.RUNNING:
        raise HTTPException(status_code=412, detail="ROS environment is not running")

    from ados.services.ros_workspace import get_info
    config = get_agent_app().config
    info = get_info(config.ros.workspace_path)
    return info


@router.post("/ros/workspace/build")
async def build_workspace() -> StreamingResponse:
    """Trigger colcon build with SSE output streaming."""
    manager = _get_manager()
    if manager.state != RosState.RUNNING:
        raise HTTPException(status_code=412, detail="ROS environment is not running")

    from ados.services.ros_workspace import build
    config = get_agent_app().config

    async def build_stream():
        try:
            async for line in build(config.ros.workspace_path):
                yield _sse_event("output", {"line": line})
            yield _sse_event("done", {"status": "success"})
        except Exception as exc:
            yield _sse_event("error", {"status": "failed", "message": str(exc)})

    return StreamingResponse(
        build_stream(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive"},
    )


@router.post("/ros/recording/start")
async def start_recording(
    topics: list[str] | None = None,
    max_size_mb: int = 500,
    max_duration_s: int = 3600,
) -> dict:
    """Start MCAP recording of ROS topics."""
    manager = _get_manager()
    if manager.state != RosState.RUNNING:
        raise HTTPException(status_code=412, detail="ROS environment is not running")

    from ados.services.ros_recording import RecordingManager
    rec_mgr = RecordingManager()
    result = rec_mgr.start(topics=topics or [], max_duration_s=max_duration_s)
    if result is None:
        raise HTTPException(status_code=500, detail="Failed to start recording")
    return result


@router.post("/ros/recording/stop")
async def stop_recording(recording_id: str = "") -> dict:
    """Stop an active MCAP recording."""
    if not recording_id:
        raise HTTPException(status_code=422, detail="recording_id is required")

    from ados.services.ros_recording import RecordingManager
    rec_mgr = RecordingManager()
    result = rec_mgr.stop(recording_id)
    if result is None:
        raise HTTPException(status_code=404, detail="Recording not found or already stopped")
    return result


@router.get("/ros/recordings")
async def list_recordings() -> list:
    """List MCAP recording files with metadata."""
    from ados.services.ros_recording import RecordingManager
    rec_mgr = RecordingManager()
    return rec_mgr.list_recordings()


# ── Tunnel stubs ────────────────────────────────────────────────────

@router.post("/ros/tunnel/config")
async def configure_tunnel() -> dict:
    """Configure cloud tunnel for ROS access. Stub."""
    raise HTTPException(status_code=501, detail="Not implemented yet")


@router.post("/ros/tunnel/test")
async def test_tunnel() -> dict:
    """Test tunnel reachability. Stub."""
    raise HTTPException(status_code=501, detail="Not implemented yet")


# ── SSE helpers ──────────────────────────────────────────────────────

def _sse_event(event_type: str, data: dict[str, Any]) -> str:
    """Format a Server-Sent Event."""
    import json
    return f"event: {event_type}\ndata: {json.dumps(data)}\n\n"
