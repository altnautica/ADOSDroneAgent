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
    1. ``config.api.mission_control_url`` if the operator set one.
    2. ``http://localhost:4000`` only when the request itself came from
       localhost / 127.0.0.1 (operator on the same machine).
    3. Empty string. The setup webapp will then say "Open Mission Control
       on your computer" rather than show a useless link.
    """
    explicit = str(getattr(config.api, "mission_control_url", "") or "")
    if explicit:
        return explicit
    if host_name in {"localhost", "127.0.0.1"}:
        return "http://localhost:4000"
    return ""


def _viewer_url_from_whep(whep_url: str | None) -> str:
    """Return the browser-clickable MediaMTX viewer URL.

    The WHEP signalling path (``/main/whep``) is for the WebRTC
    handshake — browsers cannot navigate to it directly. The viewer
    HTML page lives at ``/main/`` on the same host/port, so the
    operator-facing link points there instead. Returns "" when no
    WHEP URL is known.
    """
    if not whep_url:
        return ""
    base = whep_url
    if base.endswith("/whep"):
        base = base[: -len("/whep")]
    if not base.endswith("/"):
        base = base + "/"
    return base


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
    # The "Setup webapp" entry echoes whatever host the caller dialed (often
    # ``localhost`` when the request came from the box itself), so it is built
    # without a ``primary`` flag. The final ``_prioritize_access_urls`` pass
    # promotes the mDNS ``.local`` name (or the first LAN IP) to primary instead,
    # so a consumer never leads with a localhost URL a remote operator can't reach.
    urls: list[SetupAccessUrl] = [
        SetupAccessUrl(
            kind="setup",
            label="Setup webapp",
            url=_setup_path(base_url),
            source="local",
        ),
    ]
    if mdns_host:
        urls.append(
            SetupAccessUrl(
                kind="setup",
                label="mDNS setup",
                url=_setup_path(f"http://{mdns_host}:{port}"),
                source="mdns",
            )
        )
    urls.append(
        SetupAccessUrl(
            kind="setup", label="Hotspot setup", url=_setup_path(_HOTSPOT_URL), source="hotspot"
        )
    )
    urls.append(
        SetupAccessUrl(kind="api", label="Local API", url=f"{base_url}/api", source="local")
    )
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
    # Operator-facing link is the MediaMTX HLS viewer page at /main/.
    # The WHEP endpoint stays exposed internally for the dashboard's
    # WebRTC fast path, but advertising it as a clickable URL was a
    # dead end — browsers do not render the raw WHEP signalling URL.
    viewer_url = _viewer_url_from_whep(video.whep_url)
    if viewer_url:
        urls.append(
            SetupAccessUrl(
                kind="video",
                label="Local video viewer",
                url=viewer_url,
                source="local",
            )
        )
    public_viewer_url = _viewer_url_from_whep(video.public_whep_url)
    if public_viewer_url:
        urls.append(
            SetupAccessUrl(
                kind="video",
                label="Tunnel video viewer",
                url=public_viewer_url,
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
    return _prioritize_access_urls(_dedupe_urls(urls))


def _dedupe_urls(urls: list[SetupAccessUrl]) -> list[SetupAccessUrl]:
    seen: set[str] = set()
    unique: list[SetupAccessUrl] = []
    for item in urls:
        if item.url in seen:
            continue
        seen.add(item.url)
        unique.append(item)
    return unique


def _url_host(url: str) -> str:
    """Return the lowercased host of a URL, sans scheme, port, and path."""
    rest = url.split("://", 1)[-1]
    hostport = rest.split("/", 1)[0]
    host = hostport.rsplit(":", 1)[0] if ":" in hostport else hostport
    return host.strip().lower()


def _is_localhost_url(url: str) -> bool:
    """True when the URL points at the loopback interface (unreachable from
    any other machine on the LAN)."""
    host = _url_host(url)
    return host in ("localhost", "127.0.0.1") or host.startswith("127.")


# Sort priority for setup URLs by source. Lower sorts earlier, so the mDNS
# ``.local`` name leads, then per-NIC LAN IPs, then the hotspot / USB / tunnel
# fallbacks. A localhost entry is demoted below all of these regardless of kind.
_SETUP_SOURCE_BAND = {
    "mdns": 0,
    "local": 1,
    "hotspot": 3,
    "usb": 4,
    "cloud": 5,
}
_LOCALHOST_BAND = 90
_NON_SETUP_BAND = 50


def _url_band(item: SetupAccessUrl) -> int:
    """The sort band for one access URL (lower = earlier)."""
    if _is_localhost_url(item.url):
        return _LOCALHOST_BAND
    if item.kind == "setup":
        return _SETUP_SOURCE_BAND.get(item.source, 6)
    return _NON_SETUP_BAND


def _prioritize_access_urls(urls: list[SetupAccessUrl]) -> list[SetupAccessUrl]:
    """Order the access URLs so a consumer can lead with a reachable address.

    Priority: mDNS ``.local`` first, then per-NIC LAN IP, then the other
    reachable fallbacks, with any ``localhost`` / ``127.0.0.1`` entry demoted to
    last. The ``primary`` flag is (re)assigned to the first non-localhost setup
    URL (the ``.local`` name when present, else the first LAN IP) and never
    points at localhost when a LAN host exists. The sort is stable, so entries
    within a band keep their construction order.
    """
    ordered = sorted(urls, key=_url_band)
    for item in ordered:
        item.primary = False
    primary = next(
        (u for u in ordered if u.kind == "setup" and not _is_localhost_url(u.url)),
        None,
    )
    if primary is None:
        # No LAN-reachable setup URL at all (no mDNS name, no routable IPv4) —
        # fall back to the first setup entry so the payload still carries a
        # primary the consumer can render.
        primary = next((u for u in ordered if u.kind == "setup"), None)
    if primary is not None:
        primary.primary = True
    return ordered


__all__ = [
    "_video_access",
    "_mission_control_url",
    "_setup_path",
    "_usb_setup_url",
    "_access_urls",
    "_dedupe_urls",
    "_prioritize_access_urls",
    "_is_localhost_url",
    "_url_band",
]
