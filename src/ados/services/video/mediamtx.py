"""mediamtx subprocess start/stop lifecycle.

Config GENERATION is not here and must not come back: there is exactly one
mediamtx config generator for the whole fleet, the Rust
``ados_video::mediamtx`` renderer. This class owns only the process lifecycle
(spawn, readiness probe, stderr drain, stop) and is driven with a config path
its caller has already written — the ground-station manager renders that file
through ``ados-groundlink --emit-mediamtx-config``.
"""

from __future__ import annotations

import asyncio
import shutil
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("video.mediamtx")

_DEFAULT_API_PORT = 9997
_DEFAULT_RTSP_PORT = 8554
_DEFAULT_WEBRTC_PORT = 8889

# Max time to wait for mediamtx to bind its RTSP listener after start().
# Empirically a Pi 4B cold-start mediamtx in ~150-300ms; the prior static
# 1s sleep was usually enough but on first boot after install the load
# pushed it past 1s, so the encoder lost the race against the RTSP port
# accept and crashed with "failed to open output file rtsp://localhost:8554/main".
_RTSP_BIND_TIMEOUT_S = 10.0
_RTSP_BIND_PROBE_INTERVAL_S = 0.05


async def _wait_for_tcp_port(host: str, port: int, timeout_s: float) -> bool:
    """Poll TCP connect to (host, port) until success or timeout.

    Returns True when a connect succeeds, False on timeout. Each probe
    uses a short connect timeout so a stalled stack doesn't hold the
    loop. Used to gate downstream consumers (encoder spawn) on the
    mediamtx RTSP listener actually being ready.
    """
    deadline = asyncio.get_event_loop().time() + timeout_s
    while True:
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection(host, port),
                timeout=0.5,
            )
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass
            return True
        except (TimeoutError, OSError):
            pass
        if asyncio.get_event_loop().time() >= deadline:
            return False
        await asyncio.sleep(_RTSP_BIND_PROBE_INTERVAL_S)


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
        self._stderr_task: asyncio.Task | None = None
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
            # Drain stderr in the background to prevent pipe buffer deadlock.
            # mediamtx logs WebRTC connection events, RTSP sessions, etc. to
            # stderr. Without draining, the 64KB pipe buffer fills and mediamtx
            # blocks on its next write, freezing the entire video pipeline while
            # the process appears alive (returncode stays None, health check
            # passes). This was the root cause of progressive video freezing.
            self._stderr_task = asyncio.create_task(
                self._drain_stderr()
            )
            self._running = True
            log.info("mediamtx_started", pid=self._process.pid)
            # Block until the RTSP listener is actually accepting connections
            # so the downstream encoder doesn't lose the publish race. The
            # prior static 1s sleep was unreliable on cold-boot Pi 4B and
            # caused rpicam-vid to die with
            #   what(): failed to open output file rtsp://localhost:8554/main
            ready = await _wait_for_tcp_port(
                "127.0.0.1", _DEFAULT_RTSP_PORT, _RTSP_BIND_TIMEOUT_S,
            )
            if not ready:
                log.error(
                    "mediamtx_rtsp_port_not_ready",
                    port=_DEFAULT_RTSP_PORT,
                    timeout_s=_RTSP_BIND_TIMEOUT_S,
                )
                # Don't return False here — the process is up; the RTSP
                # listener may still come up after the timeout. Surface
                # the slow start so it can be diagnosed in journalctl.
            return True
        except Exception as exc:
            log.error("mediamtx_start_failed", error=str(exc))
            return False

    async def _drain_stderr(self) -> None:
        """Continuously drain mediamtx stderr to prevent pipe buffer deadlock."""
        if self._process is None or self._process.stderr is None:
            return
        try:
            while True:
                line = await self._process.stderr.readline()
                if not line:
                    break
                text = line.decode(errors="replace").rstrip()
                if text:
                    log.debug("mediamtx_stderr", line=text)
        except (asyncio.CancelledError, Exception):
            pass

    async def stop(self) -> None:
        """Stop the mediamtx process gracefully."""
        # Cancel stderr drain task first to avoid reading from a dead process
        if self._stderr_task is not None:
            self._stderr_task.cancel()
            self._stderr_task = None

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
