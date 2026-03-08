"""Demo video pipeline — simulated cameras and pipeline for testing without hardware."""

from __future__ import annotations

import asyncio

from ados.core.logging import get_logger
from ados.hal.camera import CameraInfo, CameraType
from ados.services.video.camera_mgr import CameraManager

log = get_logger("video.demo")

_DEMO_CAMERAS = [
    CameraInfo(
        name="Demo CSI Camera (IMX219)",
        type=CameraType.CSI,
        device_path="/dev/video0",
        width=1920,
        height=1080,
        capabilities=["h264", "mjpeg"],
    ),
    CameraInfo(
        name="Demo USB Camera (Logitech C920)",
        type=CameraType.USB,
        device_path="/dev/video2",
        width=1280,
        height=720,
        capabilities=["mjpeg", "yuyv"],
    ),
]


class DemoVideoPipeline:
    """Fake video pipeline for testing without real hardware.

    Returns mock camera lists, simulates recording state changes, and
    logs periodic status messages.  No real subprocesses are created.
    """

    def __init__(self) -> None:
        self._camera_mgr = CameraManager()
        self._camera_mgr.set_cameras(_DEMO_CAMERAS)
        self._camera_mgr.auto_assign()
        self._streaming = False
        self._recording = False
        self._recording_path = ""
        self._snapshot_count = 0

    @property
    def streaming(self) -> bool:
        return self._streaming

    @property
    def recording(self) -> bool:
        return self._recording

    @property
    def camera_manager(self) -> CameraManager:
        return self._camera_mgr

    def start_stream(self) -> bool:
        """Simulate starting the video stream."""
        self._streaming = True
        log.info("demo_stream_started")
        return True

    def stop_stream(self) -> None:
        """Simulate stopping the video stream."""
        self._streaming = False
        log.info("demo_stream_stopped")

    def start_recording(self) -> str:
        """Simulate starting a recording."""
        self._recording = True
        self._recording_path = "/tmp/ados/demo_recording.mp4"
        log.info("demo_recording_started", path=self._recording_path)
        return self._recording_path

    def stop_recording(self) -> str:
        """Simulate stopping a recording."""
        path = self._recording_path
        self._recording = False
        self._recording_path = ""
        log.info("demo_recording_stopped", path=path)
        return path

    def capture_snapshot(self) -> str:
        """Simulate capturing a snapshot."""
        self._snapshot_count += 1
        path = f"/tmp/ados/demo_snapshot_{self._snapshot_count:04d}.jpg"
        log.info("demo_snapshot_captured", path=path)
        return path

    async def run(self) -> None:
        """Main demo loop — logs periodic status updates."""
        log.info("demo_video_pipeline_start")
        self._streaming = True

        while True:
            primary = self._camera_mgr.get_primary()
            camera_name = primary.name if primary else "none"
            log.debug(
                "demo_video_status",
                streaming=self._streaming,
                recording=self._recording,
                camera=camera_name,
            )
            await asyncio.sleep(5.0)

    def get_status(self) -> dict:
        """Return current demo pipeline status."""
        return {
            "state": "running" if self._streaming else "stopped",
            "encoder": "demo",
            "cameras": self._camera_mgr.to_dict(),
            "recorder": {
                "recording": self._recording,
                "current_path": self._recording_path,
                "recordings_dir": "/tmp/ados/recordings",
            },
            "mediamtx": {
                "running": self._streaming,
                "rtsp_port": 8554,
                "webrtc_port": 8889,
                "api_port": 9997,
                "config_path": "",
            },
            "demo": True,
        }
