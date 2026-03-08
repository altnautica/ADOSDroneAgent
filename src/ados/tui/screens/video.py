"""Video screen — cameras, streams, recording status, and disk usage."""

from __future__ import annotations

import httpx
import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import DataTable, Static

log = structlog.get_logger("tui.video")


class VideoScreen(Screen):
    """Video pipeline dashboard showing cameras, streams, and recording status."""

    def compose(self) -> ComposeResult:
        with Horizontal():
            with Vertical(id="video-left"):
                yield Static("[b]Cameras[/b]", classes="panel-title")
                yield DataTable(id="camera-table")
                yield Static("[b]Role Assignments[/b]", classes="panel-title")
                yield Static("Loading...", id="roles-panel")
            with Vertical(id="video-right"):
                yield Static("[b]Pipeline Status[/b]", classes="panel-title")
                yield Static("Loading...", id="pipeline-panel")
                yield Static("[b]Recording[/b]", classes="panel-title")
                yield Static("Loading...", id="recording-panel")
                yield Static("[b]MediaMTX[/b]", classes="panel-title")
                yield Static("Loading...", id="mediamtx-panel")

    def on_mount(self) -> None:
        table = self.query_one("#camera-table", DataTable)
        table.add_columns("Name", "Type", "Device", "Resolution")
        self.set_interval(2.0, self._refresh)

    async def _refresh(self) -> None:
        api = self.app.api_url  # type: ignore[attr-defined]
        try:
            async with httpx.AsyncClient(timeout=3.0) as client:
                resp = await client.get(f"{api}/api/video")
                data = resp.json()
        except httpx.ConnectError:
            self.query_one("#pipeline-panel", Static).update("Agent not running")
            return
        except Exception as exc:
            log.warning("video_refresh_failed", error=str(exc))
            self.query_one("#pipeline-panel", Static).update("Error loading data")
            return

        # Camera table
        table = self.query_one("#camera-table", DataTable)
        table.clear()
        cameras_data = data.get("cameras", {})
        camera_list = cameras_data.get("cameras", [])
        for cam in camera_list:
            w = cam.get("width", 0)
            h = cam.get("height", 0)
            res = f"{w}x{h}" if w and h else "N/A"
            table.add_row(
                cam.get("name", "?"),
                cam.get("type", "?"),
                cam.get("device_path", "?"),
                res,
            )

        # Role assignments
        assignments = cameras_data.get("assignments", {})
        if assignments:
            lines = []
            for role, cam in assignments.items():
                lines.append(f"{role}: {cam.get('name', '?')}")
            self.query_one("#roles-panel", Static).update("\n".join(lines))
        else:
            self.query_one("#roles-panel", Static).update("No cameras assigned")

        # Pipeline status
        state = data.get("state", "unknown")
        encoder = data.get("encoder", "none")
        is_demo = data.get("demo", False)
        pipeline_text = (
            f"State:   {state}\n"
            f"Encoder: {encoder}\n"
            f"Demo:    {'yes' if is_demo else 'no'}"
        )
        self.query_one("#pipeline-panel", Static).update(pipeline_text)

        # Recording
        rec = data.get("recorder", {})
        rec_active = rec.get("recording", False)
        rec_path = rec.get("current_path", "")
        rec_dir = rec.get("recordings_dir", "")
        recording_text = (
            f"Active:  {'yes' if rec_active else 'no'}\n"
            f"Path:    {rec_path or 'N/A'}\n"
            f"Dir:     {rec_dir or 'N/A'}"
        )
        self.query_one("#recording-panel", Static).update(recording_text)

        # MediaMTX
        mtx = data.get("mediamtx", {})
        mtx_running = mtx.get("running", False)
        mtx_text = (
            f"Running: {'yes' if mtx_running else 'no'}\n"
            f"RTSP:    :{mtx.get('rtsp_port', 'N/A')}\n"
            f"WebRTC:  :{mtx.get('webrtc_port', 'N/A')}"
        )
        self.query_one("#mediamtx-panel", Static).update(mtx_text)
