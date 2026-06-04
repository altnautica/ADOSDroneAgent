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

# STUN servers for WebRTC ICE NAT traversal. Google's free public
# STUN servers handle NAT punching for ~95% of home/cellular networks.
# Required for any WAN P2P direct path; harmless on local LAN.
#
# Expanded list: more candidates means higher chance of finding a
# working ICE pair on cellular carriers, corporate networks, and NATs
# with restricted endpoint mapping. Cloudflare's STUN has global
# anycast coverage; Twilio's adds another independent network path.
# All five are free and unlimited.
_DEFAULT_STUN_SERVERS = [
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun2.l.google.com:19302",
    "stun:stun.cloudflare.com:3478",
    "stun:global.stun.twilio.com:3478",
]


def _detect_lan_ips() -> list[str]:
    """Discover the SBC's LAN IPs by enumerating non-loopback interfaces.

    mediamtx's auto-discovery of WebRTC ICE host candidates was only
    finding 127.0.0.1 on the Rock 5C Lite bench rig (probably because
    the WiFi interface comes up after mediamtx starts, or because the
    interface enumeration doesn't include all addresses). The result
    was that browsers received an SDP answer with only a loopback
    candidate and the WebRTC connection silently failed with "no video
    track received within 10s".

    Fix: detect the SBC's actual outbound IPv4 address at config-gen
    time and pass it to mediamtx as `webrtcAdditionalHosts`. This
    guarantees that at least one reachable host candidate is advertised.

    Strategy: open a UDP socket toward a public IP (no packet is
    actually sent, UDP connect is just a routing-table lookup). The
    kernel picks the outbound interface and we read its bound address.
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


def _physical_lan_interfaces() -> list[str]:
    """Names of physical wired/WiFi interfaces (eth*/en*/em*/end*, wl*/wlan*),
    excluding loopback, virtual, container, and mesh interfaces.

    Used to scope WebRTC ICE host-candidate gathering to real, reachable
    networks so the browser is never offered loopback / IPv6 link-local /
    docker / mesh candidates that just fail their connectivity checks. The
    list is read fresh on every config generation; mediamtx (Pion) re-reads
    the addresses of these interfaces per WebRTC session, so a node that moves
    from ethernet to WiFi advertises the interface that is actually up.
    """
    skip = ("lo", "docker", "veth", "br-", "bat", "tap", "tun", "wg", "virbr", "vmnet")
    names: list[str] = []
    try:
        for _idx, name in socket.if_nameindex():
            if name.startswith(skip):
                continue
            if name.startswith(("e", "w")):
                names.append(name)
    except Exception as exc:
        log.warning("phys_iface_detect_failed", error=str(exc))
    return names


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
        # Detect the SBC's LAN IP and force mediamtx to advertise it as
        # a WebRTC host candidate. Without this mediamtx auto-discovery
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
            # Bind the WebRTC media sockets to ALL interfaces (":8189") so the
            # media path is never pinned to a single IP that can disappear
            # (ethernet->WiFi failover, DHCP change, multi-homing). Gather ICE
            # host candidates only from the real physical interfaces, RE-READ
            # by Pion per session, so whatever interface is up at connect time
            # is advertised. This also fixes the original "WiFi came up after
            # mediamtx started, only 127.0.0.1 advertised" race because the
            # gather happens at connect time, not at process start. Restricting
            # to physical interfaces keeps loopback / IPv6 link-local / docker /
            # mesh junk candidates (the ones that drop after 30-60s) out.
            "webrtcIPsFromInterfaces": True,
            "webrtcIPsFromInterfacesList": _physical_lan_interfaces(),
            "webrtcHandshakeTimeout": "15s",
            "webrtcLocalUDPAddress": ":8189",
            "webrtcLocalTCPAddress": ":8189",
            "webrtcICEServers2": [
                {"url": stun_url} for stun_url in _DEFAULT_STUN_SERVERS
            ],
            # HLS is enabled so the dashboard's video player has a
            # fallback when WebRTC (WHEP) is blocked by corporate
            # networks or unsupported in the client browser. mediamtx
            # serves HLS-LL on the same port as the dashboard proxy
            # (8888 by default; see hlsAddress below).
            "hls": True,
            "hlsAddress": ":8888",
            "hlsAlwaysRemux": True,
            "hlsVariant": "lowLatency",
            "hlsSegmentCount": 7,
            "hlsSegmentDuration": "1s",
            "hlsAllowOrigin": "*",
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
