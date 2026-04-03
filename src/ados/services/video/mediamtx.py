"""mediamtx subprocess manager — config generation, start/stop lifecycle."""

from __future__ import annotations

import asyncio
import shutil
import tempfile
from pathlib import Path

import yaml

from ados.core.logging import get_logger

log = get_logger("video.mediamtx")

_DEFAULT_API_PORT = 9997
_DEFAULT_RTSP_PORT = 8554
_DEFAULT_WEBRTC_PORT = 8889


class MediamtxManager:
    """Manages a mediamtx subprocess for WebRTC/RTSP/HLS streaming.

    Generates a mediamtx.yml configuration and manages the process lifecycle.
    """

    def __init__(
        self,
        api_port: int = _DEFAULT_API_PORT,
        rtsp_port: int = _DEFAULT_RTSP_PORT,
        webrtc_port: int = _DEFAULT_WEBRTC_PORT,
    ) -> None:
        self._api_port = api_port
        self._rtsp_port = rtsp_port
        self._webrtc_port = webrtc_port
        self._process: asyncio.subprocess.Process | None = None
        self._config_path: str = ""
        self._running = False

    @property
    def running(self) -> bool:
        return self._running

    @property
    def config_path(self) -> str:
        return self._config_path

    @property
    def rtsp_port(self) -> int:
        return self._rtsp_port

    @property
    def webrtc_port(self) -> int:
        return self._webrtc_port

    def generate_config(self, streams: dict[str, str]) -> str:
        """Generate a mediamtx YAML configuration.

        Args:
            streams: Mapping of stream name to source command/URI.
                     Example: ``{"main": "rpicam-vid ... -o -"}``

        Returns:
            The path to the generated configuration file.
        """
        config: dict = {
            "logLevel": "warn",
            "api": True,
            "apiAddress": f":{self._api_port}",
            "rtsp": True,
            "rtspAddress": f":{self._rtsp_port}",
            "webrtc": True,
            "webrtcAddress": f":{self._webrtc_port}",
            "hls": False,
            "paths": {},
        }

        for name, source in streams.items():
            path_config: dict = {"source": source}
            # sourceOnDemand is only valid for non-publisher sources
            if source != "publisher":
                path_config["sourceOnDemand"] = True
            config["paths"][name] = path_config

        # Write config to a temp file
        config_dir = Path(tempfile.gettempdir()) / "ados"
        config_dir.mkdir(parents=True, exist_ok=True)
        config_path = config_dir / "mediamtx.yml"

        with open(config_path, "w") as f:
            yaml.dump(config, f, default_flow_style=False)

        self._config_path = str(config_path)
        log.info("mediamtx_config_generated", path=self._config_path, streams=list(streams))
        return self._config_path

    async def start(self) -> bool:
        """Start the mediamtx process.

        Returns True if started successfully, False if binary not found or
        already running. Waits briefly for ports to bind.
        """
        # Check if previously started process is actually still alive
        if self._running and self._process is not None:
            if self._process.returncode is not None:
                log.info("mediamtx_process_died", returncode=self._process.returncode)
                self._running = False
                self._process = None
            else:
                return True  # Already running and alive

        binary = shutil.which("mediamtx")
        if not binary:
            log.error("mediamtx_not_found", msg="mediamtx binary not in PATH")
            return False

        if not self._config_path:
            log.error("mediamtx_no_config", msg="generate_config() must be called first")
            return False

        try:
            self._process = await asyncio.create_subprocess_exec(
                binary, self._config_path,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            self._running = True
            log.info("mediamtx_started", pid=self._process.pid)
            # Wait for mediamtx to bind its ports before returning
            await asyncio.sleep(1.0)
            return True
        except Exception as exc:
            log.error("mediamtx_start_failed", error=str(exc))
            return False

    async def stop(self) -> None:
        """Stop the mediamtx process gracefully."""
        if not self._running or self._process is None:
            return

        if self._process.returncode is None:
            try:
                self._process.terminate()
            except ProcessLookupError:
                pass
            else:
                try:
                    await asyncio.wait_for(self._process.wait(), timeout=5.0)
                except TimeoutError:
                    self._process.kill()
                    await self._process.wait()

        self._running = False
        self._process = None
        log.info("mediamtx_stopped")

        # Clean up config file
        if self._config_path:
            try:
                Path(self._config_path).unlink(missing_ok=True)
            except OSError:
                pass

    def is_running(self) -> bool:
        """Check if the mediamtx process is still alive."""
        if self._process is None:
            self._running = False
            return False
        if self._process.returncode is not None:
            self._running = False
            return False
        return self._running

    def to_dict(self) -> dict:
        """Serialize state for API responses."""
        return {
            "running": self.is_running(),
            "rtsp_port": self._rtsp_port,
            "webrtc_port": self._webrtc_port,
            "api_port": self._api_port,
            "config_path": self._config_path,
        }
