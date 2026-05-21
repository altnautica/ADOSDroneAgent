"""Access-URL composer and the video-access probe.

The dashboard sidebar consumes the resulting ``SetupAccessUrl`` list to
render setup / API / video / MAVLink / Mission Control / cloud-tunnel
links. The video probe falls back to the agent's own REST surface when
the in-process pipeline is not available.
"""

from __future__ import annotations

from typing import Any

from ados.setup.models import (
    MavlinkAccess,
    RemoteAccessStatus,
    SetupAccessUrl,
    VideoAccess,
)

from ._constants import _HOTSPOT_URL, _USB_GADGET_IP


async def _video_access(runtime: Any, host_name: str) -> VideoAccess:
    """Build the VideoAccess slice with WebRTC WHEP + HLS URLs.

    HLS lives on a different mediamtx port (8888 by default) so it
    bypasses CORS and works as a fallback when WebRTC is blocked.
    The dashboard's video panel falls back to HLS when WHEP fails.
    """
    pipeline = runtime.video_pipeline()
    if pipeline is not None:
        status = pipeline.get_status()
        mtx = status.get("mediamtx", {})
        running = bool(mtx.get("running"))
        webrtc_port = int(mtx.get("webrtc_port", 8889))
        hls_port = int(mtx.get("hls_port", 8888))
        recorder = status.get("recorder", {})
        return VideoAccess(
            state="running" if running else str(status.get("state", "stopped")),
            whep_url=f"http://{host_name}:{webrtc_port}/main/whep" if running else None,
            hls_url=f"http://{host_name}:{hls_port}/main/index.m3u8" if running else None,
            recording=bool(recorder.get("recording", False)),
        )

    try:
        from ados.api.routes.video import (
            _probe_mediamtx,
            _probe_mediamtx_via_whep,
        )

        mtx = await _probe_mediamtx()
        if mtx is None or not mtx.get("ready"):
            # Ground-station-profile MediaMTX gates its management API
            # behind auth, so the JSON probe fails. The WHEP probe is
            # auth-blind and confirms the surface is serving frames.
            mtx = await _probe_mediamtx_via_whep() or mtx
        if mtx and mtx.get("ready"):
            webrtc_port = int(mtx.get("webrtc_port", 8889))
            hls_port = int(mtx.get("hls_port", 8888))
            return VideoAccess(
                state="running",
                whep_url=f"http://{host_name}:{webrtc_port}/main/whep",
                hls_url=f"http://{host_name}:{hls_port}/main/index.m3u8",
                recording=False,
            )
    except Exception:
        pass

    return VideoAccess(state="not_initialized")


def _mission_control_url(*, host_name: str, config: Any) -> str:
    """Choose a Mission Control URL to advertise.

    Priority:
    1. ``config.scripting.mission_control_url`` if the operator set one.
    2. ``http://localhost:4000`` only when the request itself came from
       localhost / 127.0.0.1 (operator on the same machine).
    3. Empty string. The setup webapp will then say "Open Mission Control
       on your computer" rather than show a useless link.
    """
    explicit = str(getattr(config.scripting, "mission_control_url", "") or "")
    if explicit:
        return explicit
    if host_name in {"localhost", "127.0.0.1"}:
        return "http://localhost:4000"
    return ""


def _setup_path(base: str) -> str:
    """Append the wizard path to a host:port base URL.

    The kind="setup" entries in access_urls are presented as "open the
    setup webapp" links in Mission Control and the local sidebar. Without
    the path, the link lands on the dashboard, so an operator who already
    finalized the wizard would get the dashboard instead of the setup
    page they asked for.
    """
    return base.rstrip("/") + "/setup"


def _usb_setup_url(*, port: int) -> str | None:
    """Best-effort USB tether setup URL.

    Only returned when the agent has actually brought up the USB gadget at
    192.168.7.1 (matched by checking the local-IPs list at call time).
    """
    return f"http://{_USB_GADGET_IP}:{port}"


def _access_urls(
    *,
    base_url: str,
    host_name: str,
    port: int,
    mdns_host: str,
    local_ips: list[str],
    video: VideoAccess,
    mavlink: MavlinkAccess,
    remote: RemoteAccessStatus,
    config: Any,
    mission_control_url: str,
) -> list[SetupAccessUrl]:
    urls = [
        SetupAccessUrl(
            kind="setup",
            label="Setup webapp",
            url=_setup_path(base_url),
            source="local",
            primary=True,
        ),
        SetupAccessUrl(
            kind="setup",
            label="mDNS setup",
            url=_setup_path(f"http://{mdns_host}:{port}"),
            source="mdns",
        ),
        SetupAccessUrl(
            kind="setup", label="Hotspot setup", url=_setup_path(_HOTSPOT_URL), source="hotspot"
        ),
        SetupAccessUrl(kind="api", label="Local API", url=f"{base_url}/api", source="local"),
    ]
    # Only advertise the USB gadget URL when the agent actually serves on
    # that IP (i.e., the gadget service has been brought up).
    if _USB_GADGET_IP in local_ips:
        usb_url = _usb_setup_url(port=port)
        if usb_url:
            urls.append(
                SetupAccessUrl(
                    kind="setup", label="USB setup", url=_setup_path(usb_url), source="usb"
                )
            )
    if mission_control_url:
        urls.append(
            SetupAccessUrl(
                kind="mission_control",
                label="Mission Control",
                url=mission_control_url,
                source="local" if host_name in {"localhost", "127.0.0.1"} else "configured",
            )
        )
    for ip in local_ips:
        urls.append(
            SetupAccessUrl(
                kind="setup",
                label=f"LAN setup {ip}",
                url=_setup_path(f"http://{ip}:{port}"),
                source="local",
            )
        )
    if video.whep_url:
        urls.append(
            SetupAccessUrl(
                kind="video",
                label="Local WHEP video",
                url=video.whep_url,
                source="local",
            )
        )
    if video.public_whep_url:
        urls.append(
            SetupAccessUrl(
                kind="video",
                label="Tunnel WHEP video",
                url=video.public_whep_url,
                source="cloud",
            )
        )
    if mavlink.websocket_url:
        urls.append(
            SetupAccessUrl(
                kind="mavlink",
                label="MAVLink WebSocket",
                url=mavlink.websocket_url,
                source="local",
            )
        )
    if mavlink.public_websocket_url:
        urls.append(
            SetupAccessUrl(
                kind="mavlink",
                label="Tunnel MAVLink WebSocket",
                url=mavlink.public_websocket_url,
                source="cloud",
            )
        )
    if config.remote_access.cloudflare.setup_url:
        urls.append(
            SetupAccessUrl(
                kind="setup",
                label="Tunnel setup",
                url=_setup_path(config.remote_access.cloudflare.setup_url),
                source="cloud",
            )
        )
    for url in remote.public_urls:
        urls.append(SetupAccessUrl(kind="cloud", label="Remote access", url=url, source="cloud"))
    return _dedupe_urls(urls)


def _dedupe_urls(urls: list[SetupAccessUrl]) -> list[SetupAccessUrl]:
    seen: set[str] = set()
    unique: list[SetupAccessUrl] = []
    for item in urls:
        if item.url in seen:
            continue
        seen.add(item.url)
        unique.append(item)
    return unique


__all__ = [
    "_video_access",
    "_mission_control_url",
    "_setup_path",
    "_usb_setup_url",
    "_access_urls",
    "_dedupe_urls",
]
