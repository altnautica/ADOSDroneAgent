"""mediamtx subprocess manager — config generation, start/stop lifecycle."""

from __future__ import annotations

import asyncio
import shutil
import socket
import tempfile
from pathlib import Path

import yaml

from ados.core.logging import get_logger

log = get_logger("video.mediamtx")

_DEFAULT_API_PORT = 9997
_DEFAULT_RTSP_PORT = 8554
_DEFAULT_WEBRTC_PORT = 8889

# DEC-108: STUN servers for WebRTC ICE NAT traversal. Google's free public
# STUN servers handle NAT punching for ~95% of home/cellular networks.
# Required for any WAN P2P direct path; harmless on local LAN.
#
# DEC-107 Phase H: expanded list — more candidates = higher chance of finding
# a working ICE pair on cellular carriers, corporate networks, and NATs with
# restricted endpoint mapping. Cloudflare's STUN has global anycast coverage;
# Twilio's adds another independent network path. All five are free and
# unlimited.
_DEFAULT_STUN_SERVERS = [
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun2.l.google.com:19302",
    "stun:stun.cloudflare.com:3478",
    "stun:global.stun.twilio.com:3478",
]


def _detect_lan_ips() -> list[str]:
    """Discover the SBC's LAN IPs by enumerating non-loopback interfaces.

    DEC-108: mediamtx's auto-discovery of WebRTC ICE host candidates was
    only finding 127.0.0.1 on the Rock 5C Lite bench rig (probably because
    the WiFi interface comes up after mediamtx starts, or because the
    interface enumeration doesn't include all addresses). The result was
    that browsers received an SDP answer with only a loopback candidate
    and the WebRTC connection silently failed with "no video track
    received within 10s".

    Fix: detect the SBC's actual outbound IPv4 address at config-gen time
    and pass it to mediamtx as `webrtcAdditionalHosts`. This guarantees
    that at least one reachable host candidate is advertised.

    Strategy: open a UDP socket toward a public IP (no packet is actually
    sent — UDP connect is just a routing-table lookup). The kernel picks
    the outbound interface and we read its bound address.
    """
    ips: list[str] = []
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        if ip and ip != "127.0.0.1" and not ip.startswith("169.254."):
            ips.append(ip)
    except Exception as exc:
        log.warning("lan_ip_detect_failed", error=str(exc))
    return ips


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

    def generate_config(self, streams: dict[str, str]) -> str:
        """Generate a mediamtx YAML configuration.

        Args:
            streams: Mapping of stream name to source command/URI.
                     Example: ``{"main": "rpicam-vid ... -o -"}``

        Returns:
            The path to the generated configuration file.
        """
        # DEC-108: detect the SBC's LAN IP and force mediamtx to advertise
        # it as a WebRTC host candidate. Without this mediamtx auto-discovery
        # was only emitting 127.0.0.1, which browsers can't reach.
        lan_ips = _detect_lan_ips()
        log.info("mediamtx_webrtc_hosts", hosts=lan_ips)

        config: dict = {
            "logLevel": "warn",
            "api": True,
            "apiAddress": f":{self._api_port}",
            "rtsp": True,
            "rtspAddress": f":{self._rtsp_port}",
            "webrtc": True,
            "webrtcAddress": f":{self._webrtc_port}",
            "webrtcAllowOrigin": "*",
            # Disable interface IP auto-discovery AND bind UDP/TCP to the
            # specific IPv4 LAN address. Without this, Pion's ICE agent
            # enumerates all socket-bound addresses (127.0.0.1, IPv6 ULA,
            # link-local) and the browser selects unstable candidates that
            # drop after 30-60s. Binding to the LAN IP forces ONLY that
            # address as an ICE host candidate.
            "webrtcIPsFromInterfaces": False,
            "webrtcIPsFromInterfacesList": [],
            "webrtcHandshakeTimeout": "15s",
            "webrtcLocalUDPAddress": f"{lan_ips[0]}:8189" if lan_ips else ":8189",
            "webrtcLocalTCPAddress": f"{lan_ips[0]}:8189" if lan_ips else ":8189",
            "webrtcICEServers2": [
                {"url": stun_url} for stun_url in _DEFAULT_STUN_SERVERS
            ],
            "hls": False,
            "paths": {},
        }
        if lan_ips:
            config["webrtcAdditionalHosts"] = lan_ips

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
            # Wait for mediamtx to bind its ports before returning
            await asyncio.sleep(1.0)
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
