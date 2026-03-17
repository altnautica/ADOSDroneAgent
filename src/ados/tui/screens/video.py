"""Video screen — cameras, streams, recording status, and disk usage."""

from __future__ import annotations

import structlog
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import DataTable, Static

from ados.tui.widgets import GaugeBar, InfoPanel, StatusDot

log = structlog.get_logger("tui.video")


class VideoScreen(Screen):
    """Video pipeline dashboard showing cameras, streams, and recording status."""

    def compose(self) -> ComposeResult:
        with Horizontal():
            with Vertical(id="video-left"):
                with InfoPanel("CAMERAS"):
                    yield DataTable(id="camera-table")
                with InfoPanel("ROLE ASSIGNMENTS"):
                    yield Static("Loading...", id="roles-panel")
            with Vertical(id="video-right"):
                with InfoPanel("PIPELINE"):
                    yield StatusDot("Pipeline", "unknown", id="pipeline-dot")
                    yield Static("", id="pipeline-detail")
                with InfoPanel("RECORDING"):
                    yield StatusDot("Recording", "idle", id="rec-dot")
                    yield Static("", id="recording-detail")
                    yield GaugeBar(
                        label="Disk",
                        value=0,
                        thresholds=(70.0, 90.0),
                        id="disk-gauge",
                    )
                with InfoPanel("MEDIAMTX"):
                    yield StatusDot("MediaMTX", "unknown", id="mtx-dot")
                    yield Static("", id="mediamtx-detail")

    def on_mount(self) -> None:
        table = self.query_one("#camera-table", DataTable)
        table.add_columns("Name", "Type", "Device", "Resolution")
        self.set_interval(2.0, self._refresh)

    async def _refresh(self) -> None:
        fetcher = self.app.fetcher  # type: ignore[attr-defined]
        data = await fetcher.get_video()

        if data is None:
            self.query_one("#pipeline-dot", StatusDot).set_state("disconnected")
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

        # Map state to StatusDot states
        state_map = {
            "streaming": "running",
            "running": "running",
            "active": "active",
            "stopped": "stopped",
            "error": "error",
            "idle": "idle",
        }
        self.query_one("#pipeline-dot", StatusDot).set_state(
            state_map.get(state, "unknown")
        )
        self.query_one("#pipeline-detail", Static).update(
            f"Encoder: {encoder}\n"
            f"Demo:    {'yes' if is_demo else 'no'}"
        )

        # Recording
        rec = data.get("recorder", {})
        rec_active = rec.get("recording", False)
        rec_path = rec.get("current_path", "")
        rec_dir = rec.get("recordings_dir", "")
        self.query_one("#rec-dot", StatusDot).set_state(
            "active" if rec_active else "idle"
        )
        self.query_one("#recording-detail", Static).update(
            f"Path:    {rec_path or 'N/A'}\n"
            f"Dir:     {rec_dir or 'N/A'}"
        )

        # Disk usage gauge (if available in recorder data)
        disk_pct = rec.get("disk_usage_percent", 0)
        self.query_one("#disk-gauge", GaugeBar).update_value(disk_pct)

        # MediaMTX
        mtx = data.get("mediamtx", {})
        mtx_running = mtx.get("running", False)
        self.query_one("#mtx-dot", StatusDot).set_state(
            "running" if mtx_running else "stopped"
        )
        self.query_one("#mediamtx-detail", Static).update(
            f"RTSP:    :{mtx.get('rtsp_port', 'N/A')}\n"
            f"WebRTC:  :{mtx.get('webrtc_port', 'N/A')}"
        )
