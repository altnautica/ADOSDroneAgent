"""ROS 2 environment management API routes.

DEC-111: REST endpoints for initializing, monitoring, and controlling
the opt-in ROS 2 Jazzy Docker environment.

Phase 2 endpoints: /api/ros/status, /api/ros/init, /api/ros/nodes, /api/ros/topics
Phase 4 endpoints (stubs): launch, stop, workspace, build, recording, tunnel
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


# ── Phase 4 stubs (workspace, launch, recording, tunnel) ────────────

@router.post("/ros/launch")
async def launch_node(package: str = "", executable: str = "", name: str = "") -> dict:
    """Launch a user ROS node. Phase 4."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 4)")


@router.get("/ros/workspace")
async def get_workspace() -> dict:
    """Get workspace metadata. Phase 4."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 4)")


@router.post("/ros/workspace/build")
async def build_workspace() -> StreamingResponse:
    """Trigger colcon build. Phase 4."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 4)")


@router.post("/ros/recording/start")
async def start_recording() -> dict:
    """Start MCAP recording. Phase 4."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 4)")


@router.post("/ros/recording/stop")
async def stop_recording() -> dict:
    """Stop MCAP recording. Phase 4."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 4)")


@router.get("/ros/recordings")
async def list_recordings() -> list:
    """List MCAP recordings. Phase 4."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 4)")


@router.post("/ros/tunnel/config")
async def configure_tunnel() -> dict:
    """Configure cloud tunnel for ROS access. Phase 5."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 5)")


@router.post("/ros/tunnel/test")
async def test_tunnel() -> dict:
    """Test tunnel reachability. Phase 5."""
    raise HTTPException(status_code=501, detail="Not implemented yet (Phase 5)")


# ── SSE helpers ──────────────────────────────────────────────────────

def _sse_event(event_type: str, data: dict[str, Any]) -> str:
    """Format a Server-Sent Event."""
    import json
    return f"event: {event_type}\ndata: {json.dumps(data)}\n\n"
