"""Universal setup status assembly and remote-access helpers."""

from __future__ import annotations

import os
import re
import shutil
import socket
import subprocess
from pathlib import Path
from typing import Any, Literal

from ados import __version__
from ados.setup.hardware_check import derive_step_state, run_hardware_check
from ados.setup.models import (
    CloudChoiceStatus,
    HardwareCheckStatus,
    MavlinkAccess,
    NetworkStatus,
    ProfileSuggestion,
    RemoteAccessStatus,
    ServiceState,
    SetupAccessUrl,
    SetupActionResult,
    SetupStatus,
    SetupStep,
    VideoAccess,
)
from ados.setup.profile import build_profile_suggestion
from ados.setup.state import read_state

# Canonical local-access endpoints. These mirror the addresses configured by
# `services.network.wifi_ap` (hotspot AP) and `services.ground_station.usb_gadget`
# (RNDIS / CDC-NCM USB tether). Keep in sync with those modules.
_HOTSPOT_IP = "192.168.4.1"
_USB_GADGET_IP = "192.168.7.1"
_HOTSPOT_URL = f"http://{_HOTSPOT_IP}"
_USB_URL_TEMPLATE = "http://{ip}:{port}"
_TOKEN_RE = re.compile(r"(?:--token|service\s+install)\s+['\"]?([^'\"\s]+)")


def _hostname() -> str:
    try:
        return socket.gethostname()
    except OSError:
        return ""


def _local_ips() -> list[str]:
    ips: set[str] = set()
    try:
        import psutil  # type: ignore[import-untyped]

        for addrs in psutil.net_if_addrs().values():
            for addr in addrs:
                if getattr(addr, "family", None) == socket.AF_INET:
                    value = str(getattr(addr, "address", ""))
                    if value and not value.startswith("127."):
                        ips.add(value)
    except Exception:
        pass

    if not ips:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
                sock.connect(("8.8.8.8", 80))
                ips.add(sock.getsockname()[0])
        except OSError:
            pass

    return sorted(ips)


def _first_mavlink_ws_port(config: Any) -> int:
    for endpoint in getattr(config.mavlink, "endpoints", []):
        if getattr(endpoint, "type", "") == "websocket" and getattr(endpoint, "enabled", False):
            return int(getattr(endpoint, "port", 8765))
    return 8765


def _services(runtime: Any) -> list[ServiceState]:
    tracker = runtime.service_tracker
    data = tracker.to_dict() if tracker else {}
    rows: dict[str, ServiceState] = {}
    for name, info in data.items():
        raw_state = info.get("state")
        state = getattr(raw_state, "value", raw_state) or "unknown"
        rows[name] = ServiceState(name=name, state=str(state))
    for task in runtime.service_tasks():
        name = task.get_name()
        if name in rows:
            continue
        rows[name] = ServiceState(
            name=name,
            state="running" if not task.done() else "stopped",
        )
    return sorted(rows.values(), key=lambda svc: svc.name)


def _service_state(services: list[ServiceState], name: str) -> str:
    for service in services:
        if service.name == name:
            return service.state
    return ""


def _remote_status(config: Any) -> RemoteAccessStatus:
    remote = config.remote_access
    cf = remote.cloudflare
    public_urls = list(remote.public_urls)
    for url in (cf.setup_url, cf.api_url, cf.video_whep_url, cf.mavlink_ws_url):
        if url and url not in public_urls:
            public_urls.append(url)

    configured = bool(cf.enabled and Path(cf.token_path).is_file())
    status: Literal["disabled", "configured", "running", "stopped", "error"] = "disabled"
    error = ""
    if cf.enabled:
        status = "configured" if configured else "error"
        if not configured:
            error = "Cloudflare tunnel is enabled but no token is installed"
        elif _cloudflared_running(cf.service_name):
            status = "running"
        else:
            status = "stopped"

    return RemoteAccessStatus(
        provider=remote.provider,
        enabled=bool(cf.enabled),
        configured=configured,
        status=status,
        public_urls=public_urls,
        error=error,
    )


def _cloudflared_running(service_name: str) -> bool:
    if shutil.which("systemctl"):
        try:
            result = subprocess.run(
                ["systemctl", "is-active", "--quiet", service_name],
                capture_output=True,
                timeout=3,
            )
            return result.returncode == 0
        except (OSError, subprocess.SubprocessError):
            return False
    return False


def _resolve_display_step(
    hardware_check: HardwareCheckStatus,
) -> tuple[str, str]:
    """Map the hardware-check ``display`` row onto wizard step state + detail.

    Returns ``(state, detail)`` where ``state`` is one of the
    ``SetupStepState`` literal values. The wizard step is the action
    surface; the table row in the hardware-check step provides the same
    information as a diagnostic readout, so both stay in sync because
    they read from the same probe.
    """
    item = next(
        (i for i in hardware_check.items if i.id == "display"),
        None,
    )
    if item is None:
        return "needs_action", "Plug a supported SPI LCD to configure local display."
    detail = item.detail or item.label
    # Probe states defined in `_check_display`:
    #   - ok: configured + bound + driver matches
    #   - warning: configured but fb1 absent OR driver mismatch
    #   - unknown: no display.conf at all (or display_id=none after skip)
    if item.state == "ok":
        return "complete", detail
    if item.state == "unknown":
        # Check whether the operator explicitly skipped (display_id=none).
        # The hardware-check probe reports state="unknown" both for
        # "never configured" and for the explicit-skip path; the wizard
        # distinguishes via /etc/ados/display.conf.
        from ados.core.paths import DISPLAY_CONF_PATH

        if DISPLAY_CONF_PATH.exists():
            try:
                text = DISPLAY_CONF_PATH.read_text()
            except OSError:
                text = ""
            if "display_id=none" in text:
                return "optional", "Display step skipped"
        return "needs_action", detail or "No local display configured"
    if item.state == "warning":
        return "needs_action", detail
    return "needs_action", detail or "Configure local display"


def _setup_steps(
    *,
    profile: str,
    mavlink: MavlinkAccess,
    video: VideoAccess,
    network: NetworkStatus,
    remote: RemoteAccessStatus,
    cloud_choice: CloudChoiceStatus,
    profile_suggestion: ProfileSuggestion,
    hardware_check: HardwareCheckStatus,
    mission_control_url: str,
) -> list[SetupStep]:
    """Emit the canonical onboarding steps in spec-39 order.

    Profile branches drop steps that do not apply: the drone profile has
    no ``ground_receiver`` step; the ground profile has no ``mavlink``
    step. The ``profile`` step is profile-agnostic and lets the operator
    confirm or override the auto-detected fingerprint. The
    ``hardware_check`` step renders a per-component readout for the
    chosen profile.
    """
    is_drone = profile != "ground_station"
    is_ground = profile == "ground_station"
    network_complete = bool(network.local_ips) or bool(network.hotspot_enabled)
    cloud_paired = cloud_choice.paired
    cloud_local = cloud_choice.mode == "local"
    profile_confirmed = profile_suggestion.confirmed and profile in (
        "drone",
        "ground_station",
    )
    hw_state, hw_detail = derive_step_state(hardware_check)

    steps: list[SetupStep] = []

    steps.append(
        SetupStep(
            id="welcome",
            label="Welcome",
            state="complete",
            detail="Device identity available",
        )
    )

    steps.append(
        SetupStep(
            id="profile",
            label="Profile",
            state="complete" if profile_confirmed else "needs_action",
            detail=(
                f"Confirmed as {profile}"
                if profile_confirmed
                else "Confirm or change the profile for this device"
            ),
            action_label="Choose profile",
            href="/setup.html?step=profile",
        )
    )

    # Network readout used to be its own step. The wizard's welcome step
    # now surfaces the same data inline as a chip row, so we no longer
    # render a dedicated network step. The /network.html surface stays as
    # the standalone diagnostic page; only the wizard step is dropped.
    # The network_complete signal is still computed above and consumed by
    # the welcome step state below.

    # Welcome state is upgraded to needs_action when there is no usable
    # local network so the operator does not coast past the chip row.
    if not network_complete and steps:
        steps[0] = SetupStep(
            id="welcome",
            label="Welcome",
            state="needs_action",
            detail="Bring up Wi-Fi, hotspot, USB tether, or LAN to continue.",
        )

    steps.append(
        SetupStep(
            id="hardware_check",
            label="Hardware check",
            state=hw_state,  # type: ignore[arg-type]
            detail=hw_detail,
            action_label="Open hardware check",
            href="/setup.html?step=hardware_check",
        )
    )

    steps.append(
        SetupStep(
            id="cloud_choice",
            label="Cloud posture",
            state=(
                "complete"
                if cloud_choice.mode in ("cloud", "self_hosted", "local")
                and (cloud_local or cloud_choice.backend_url)
                else "needs_action"
            ),
            detail=(
                "Local-only mode. No cloud relay configured."
                if cloud_local
                else f"Connected to {cloud_choice.backend_url}"
                if cloud_choice.backend_url
                else "Choose a cloud posture for this device"
            ),
            action_label="Choose cloud posture",
            href="/setup.html?step=cloud_choice",
        )
    )

    # Pair step is only meaningful when the device is set up to talk to a
    # cloud or self-hosted backend. Local-only deployments hide it entirely
    # so the wizard does not waste an operator's attention on a step they
    # have nothing to do on.
    if not cloud_local:
        steps.append(
            SetupStep(
                id="pair",
                label="Pair with Mission Control",
                state="complete" if cloud_paired else "needs_action",
                detail=(
                    "Device is paired."
                    if cloud_paired
                    else "Show this device's code or accept one from Mission Control."
                ),
                action_label="Pair this device",
                href="/setup.html?step=pair",
            )
        )

    if is_drone:
        steps.append(
            SetupStep(
                id="mavlink",
                label="Flight controller",
                state="complete" if mavlink.connected else "needs_action",
                detail=(
                    "MAVLink telemetry is live"
                    if mavlink.connected
                    else "Connect or configure the flight controller"
                ),
                action_label="Open MAVLink",
                href="/mavlink.html",
            )
        )

    # Sharper detail string when a camera is detected by the HAL but the
    # pipeline has not yet reached running state. Helps the operator see
    # that the agent IS aware of their hardware so they don't think the
    # camera is dead.
    camera_detected = any(
        item.id == "camera" and item.state == "ok"
        for item in hardware_check.items
    )
    if video.state == "running":
        video_detail = "WHEP video is live"
    elif camera_detected:
        video_detail = "Camera detected. Click Start video to begin streaming."
    else:
        video_detail = "No camera or receiver detected. Skip if you do not need video on this device."

    steps.append(
        SetupStep(
            id="video",
            label="Video",
            state="complete" if video.state == "running" else "needs_action",
            detail=video_detail,
            action_label="Open Video",
            href="/video.html",
        )
    )

    if is_ground:
        steps.append(
            SetupStep(
                id="ground_receiver",
                label="Ground receiver",
                state="complete" if video.state == "running" else "needs_action",
                detail="WFB receiver and mesh role configuration",
                action_label="Open Ground station",
                href="/ground.html",
            )
        )
        # Local-display step. Surfaces an SPI LCD attached over the
        # 40-pin expansion header (Waveshare 3.5" RPi LCD on Cubie A7Z
        # or Rock 5C ground-station builds). State is derived from the
        # same hardware-check item that the diagnostic table uses, so
        # the two surfaces stay in sync. Hidden on drone profile
        # because no LCD path exists on air-side rigs in this revision.
        display_state, display_detail = _resolve_display_step(hardware_check)
        steps.append(
            SetupStep(
                id="display",
                label="Local display",
                state=display_state,  # type: ignore[arg-type]
                detail=display_detail,
                action_label="Configure display",
                href="/setup.html?step=display",
            )
        )

    steps.append(
        SetupStep(
            id="remote_access",
            label="Remote access",
            state="complete" if remote.status == "running" else "optional",
            detail=(
                "Cloudflare tunnel is running"
                if remote.status == "running"
                else "Optional cloud or tunnel link"
            ),
            action_label="Open Remote access",
            href="/remote.html",
        )
    )

    steps.append(
        SetupStep(
            id="finish",
            label="Finish",
            state="complete" if mavlink.connected or video.state == "running" else "optional",
            detail=(
                "Open Mission Control when local telemetry or video is ready"
                if mission_control_url
                else "Open Mission Control on your computer once telemetry or video is ready"
            ),
            action_label="Open Mission Control" if mission_control_url else "",
            href=mission_control_url,
        )
    )

    return steps


def _build_known_hosts(
    *,
    local_ips: list[str],
    mdns_host: str,
    config: Any,
) -> set[str]:
    """The set of host strings the agent will accept in a Host header.

    Used to reject Host-header injection from a hostile upstream proxy. We
    accept localhost, the configured mDNS host, every discovered local IP,
    and the hotspot/USB-gadget addresses the agent itself binds.
    """
    hosts: set[str] = {"localhost", "127.0.0.1", _HOTSPOT_IP, _USB_GADGET_IP}
    if mdns_host:
        hosts.add(mdns_host)
    hostname = _hostname()
    if hostname:
        hosts.add(hostname)
        hosts.add(f"{hostname}.local")
    for ip in local_ips:
        hosts.add(ip)
    cf = getattr(config, "remote_access", None)
    if cf is not None:
        cloudflare = getattr(cf, "cloudflare", None)
        for url in (
            getattr(cloudflare, "setup_url", "") if cloudflare else "",
            getattr(cloudflare, "api_url", "") if cloudflare else "",
        ):
            if url:
                try:
                    parsed_host = url.split("://", 1)[-1].split("/", 1)[0].split(":", 1)[0]
                    if parsed_host:
                        hosts.add(parsed_host)
                except Exception:
                    pass
    return hosts


def _safe_host_for(host_header: str | None, known_hosts: set[str]) -> str:
    """Validate a Host header against known-good hosts.

    Returns ``host:port`` when the header carries a host the agent itself
    advertises; otherwise falls back to ``localhost:8080``. Multi-value
    chains (proxy lists) take only the first entry.
    """
    if not host_header:
        return "localhost:8080"
    candidate = host_header.split(",")[0].strip()
    if not candidate:
        return "localhost:8080"
    host_only = candidate.split(":", 1)[0]
    if host_only and host_only in known_hosts:
        return candidate
    return "localhost:8080"


async def build_setup_status(runtime: Any, host_header: str | None = None) -> SetupStatus:
    """Build a complete setup status document from the API runtime facade."""
    config = runtime.config
    port = int(getattr(config.scripting.rest_api, "port", 8080))
    local_ips = _local_ips()
    mdns_host = "ados.local"
    if config.agent.device_id:
        mdns_host = f"ados-{config.agent.device_id}.local"
    known_hosts = _build_known_hosts(
        local_ips=local_ips, mdns_host=mdns_host, config=config
    )
    host = _safe_host_for(host_header, known_hosts)
    host_name = host.split(":")[0]
    base_url = f"http://{host}"

    fc = runtime.fc_status()
    mavlink_ws_port = _first_mavlink_ws_port(config)
    mavlink_url = f"ws://{host_name}:{mavlink_ws_port}/"

    video = await _video_access(runtime, host_name)
    remote = _remote_status(config)
    network = NetworkStatus(
        hostname=_hostname(),
        mdns_host=mdns_host,
        api_port=port,
        hotspot_enabled=bool(config.network.hotspot.enabled),
        hotspot_ssid=str(config.network.hotspot.ssid).replace(
            "{device_id}", config.agent.device_id or "device"
        ),
        local_ips=local_ips,
    )
    mavlink = MavlinkAccess(
        connected=fc.connected,
        port=str(fc.port or ""),
        baud=int(fc.baud or 0) if fc.baud is not None else None,
        websocket_url=mavlink_url,
        public_websocket_url=config.remote_access.cloudflare.mavlink_ws_url or None,
    )
    if video.public_whep_url is None and config.remote_access.cloudflare.video_whep_url:
        video.public_whep_url = config.remote_access.cloudflare.video_whep_url

    services = _services(runtime)
    cloud_choice = _cloud_choice_status(config)
    profile_suggestion = build_profile_suggestion(config)
    profile_for_check = str(config.agent.profile)
    if profile_for_check == "auto":
        profile_for_check = (
            profile_suggestion.detected
            if profile_suggestion.detected != "unconfigured"
            else "drone"
        )
    ground_role = str(getattr(config.ground_station, "role", "direct") or "direct")
    hardware_check = run_hardware_check(
        runtime,
        profile=profile_for_check,
        ground_role=ground_role,
    )
    persisted = read_state()
    mission_control_url = _mission_control_url(host_name=host_name, config=config)
    access_urls = _access_urls(
        base_url=base_url,
        host_name=host_name,
        port=port,
        mdns_host=mdns_host,
        local_ips=local_ips,
        video=video,
        mavlink=mavlink,
        remote=remote,
        config=config,
        mission_control_url=mission_control_url,
    )
    steps = _setup_steps(
        profile=str(config.agent.profile),
        mavlink=mavlink,
        video=video,
        network=network,
        remote=remote,
        cloud_choice=cloud_choice,
        profile_suggestion=profile_suggestion,
        hardware_check=hardware_check,
        mission_control_url=mission_control_url,
    )

    # Apply persisted skip flags. Steps the operator chose to defer move
    # from `needs_action` to `optional` so they no longer block the
    # `setup_complete` derivation. We never downgrade `complete` or
    # `not_applicable` via skip.
    if persisted.skipped_steps:
        for step in steps:
            if step.id in persisted.skipped_steps and step.state == "needs_action":
                step.state = "optional"

    complete_steps = sum(1 for step in steps if step.state == "complete")
    completion_percent = round((complete_steps / len(steps)) * 100)
    next_step = next((step for step in steps if step.state == "needs_action"), None)
    next_action = (
        next_step.detail
        if next_step
        else "Open Mission Control or continue optional remote access setup"
    )

    natural_complete = not any(step.state == "needs_action" for step in steps)
    setup_complete = persisted.setup_finalized or natural_complete

    return SetupStatus(
        version=__version__,
        device_id=config.agent.device_id,
        device_name=config.agent.name,
        profile=config.agent.profile,
        ground_role=ground_role,
        setup_complete=setup_complete,
        setup_finalized=persisted.setup_finalized,
        completion_percent=completion_percent,
        next_action=next_action,
        steps=steps,
        access_urls=access_urls,
        network=network,
        mavlink=mavlink,
        video=video,
        remote_access=remote,
        services=services,
        telemetry=runtime.vehicle_state_dict(),
        cloud_choice=cloud_choice,
        profile_suggestion=profile_suggestion,
        hardware_check=hardware_check,
        skipped_steps=sorted(persisted.skipped_steps),
    )


async def _video_access(runtime: Any, host_name: str) -> VideoAccess:
    pipeline = runtime.video_pipeline()
    if pipeline is not None:
        status = pipeline.get_status()
        mtx = status.get("mediamtx", {})
        running = bool(mtx.get("running"))
        webrtc_port = int(mtx.get("webrtc_port", 8889))
        recorder = status.get("recorder", {})
        return VideoAccess(
            state="running" if running else str(status.get("state", "stopped")),
            whep_url=f"http://{host_name}:{webrtc_port}/main/whep" if running else None,
            recording=bool(recorder.get("recording", False)),
        )

    try:
        from ados.api.routes.video import _probe_mediamtx

        mtx = await _probe_mediamtx()
        if mtx and mtx.get("ready"):
            webrtc_port = int(mtx.get("webrtc_port", 8889))
            return VideoAccess(
                state="running",
                whep_url=f"http://{host_name}:{webrtc_port}/main/whep",
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
    return base.rstrip("/") + "/setup.html"


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


def extract_cloudflare_token(value: str) -> str:
    """Extract a tunnel token from a raw token or Cloudflare install command."""
    candidate = value.strip()
    match = _TOKEN_RE.search(candidate)
    if match:
        candidate = match.group(1)
    candidate = candidate.strip().strip("'\"")
    if not candidate or any(ch.isspace() for ch in candidate):
        raise ValueError("Cloudflare tunnel token could not be found")
    if len(candidate) < 20:
        raise ValueError("Cloudflare tunnel token is too short")
    return candidate


def _cloud_choice_status(config: Any) -> CloudChoiceStatus:
    """Read the current cloud posture out of config for display."""
    server = getattr(config, "server", None)
    mode = getattr(server, "mode", "cloud") if server else "cloud"
    if mode not in ("cloud", "self_hosted", "local"):
        mode = "cloud"
    if mode == "local":
        return CloudChoiceStatus(
            mode="local",
            paired=False,
            pair_code_required=False,
            backend_url="",
            backend_reachable=False,
        )
    if mode == "self_hosted":
        sh = getattr(server, "self_hosted", None)
        url = str(getattr(sh, "url", "") or "")
        return CloudChoiceStatus(
            mode="self_hosted",
            paired=bool(getattr(sh, "api_key", "") or ""),
            pair_code_required=True,
            backend_url=url,
            backend_reachable=False,
        )
    cloud = getattr(server, "cloud", None)
    cloud_url = str(getattr(cloud, "url", "") or "")
    return CloudChoiceStatus(
        mode="cloud",
        paired=False,
        pair_code_required=True,
        backend_url=cloud_url,
        backend_reachable=False,
    )


def apply_cloud_choice(
    runtime: Any,
    *,
    mode: str,
    self_hosted: dict[str, Any] | None = None,
) -> SetupActionResult:
    """Apply a cloud-posture choice to ``config.server``.

    Persists the chosen mode and any self-hosted backend coordinates the
    operator entered. The optional ``api_key`` is written to a root-owned
    secret file and is not stored back in config or returned in the
    response. ``mqtt_password`` is cleared on transition to ``local``.
    """
    if mode not in ("cloud", "self_hosted", "local"):
        return SetupActionResult(ok=False, message=f"Unknown mode: {mode}")

    if mode == "self_hosted":
        if not self_hosted or not self_hosted.get("url"):
            return SetupActionResult(
                ok=False,
                message="self_hosted.url is required when mode is 'self_hosted'",
            )
    elif self_hosted:
        return SetupActionResult(
            ok=False,
            message="self_hosted block is only valid when mode is 'self_hosted'",
        )

    config = runtime.config
    config.server.mode = mode

    api_key_written = False
    if mode == "self_hosted":
        sh = config.server.self_hosted
        sh.url = str(self_hosted.get("url") or "").strip()
        sh.mqtt_broker = str(self_hosted.get("mqtt_broker") or "").strip()
        port_raw = self_hosted.get("mqtt_port")
        if port_raw is not None:
            try:
                port_int = int(port_raw)
            except (TypeError, ValueError):
                return SetupActionResult(
                    ok=False, message="self_hosted.mqtt_port must be an integer"
                )
            if not (1 <= port_int <= 65535):
                return SetupActionResult(
                    ok=False, message="self_hosted.mqtt_port must be 1-65535"
                )
            sh.mqtt_port = port_int
        api_key = self_hosted.get("api_key")
        if api_key:
            try:
                from ados.core.paths import SERVER_API_KEY_PATH
                SERVER_API_KEY_PATH.parent.mkdir(parents=True, exist_ok=True)
                fd = os.open(
                    str(SERVER_API_KEY_PATH),
                    os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
                    0o600,
                )
                with os.fdopen(fd, "w", encoding="utf-8") as fh:
                    fh.write(str(api_key).strip())
                    fh.write("\n")
                api_key_written = True
                sh.api_key = ""  # never echo back through config
            except OSError as exc:
                return SetupActionResult(
                    ok=False, message=f"Could not write API key: {exc}"
                )

    if mode == "local":
        config.server.mqtt_password = ""

    saver = getattr(runtime.raw_runtime, "save_config", None)
    if callable(saver):
        try:
            saver()
        except Exception:
            pass

    data: dict[str, object] = {
        "mode": mode,
        "api_key_written": api_key_written,
    }
    if mode == "cloud":
        data["backend_url"] = config.server.cloud.url
    elif mode == "self_hosted":
        data["backend_url"] = config.server.self_hosted.url

    if mode == "local":
        message = "Cloud posture set to local-only. Mission Control connects directly."
    elif mode == "cloud":
        message = "Cloud posture set to Altnautica cloud. Continue to pairing."
    else:
        message = "Cloud posture set to self-hosted backend. Continue to pairing."

    return SetupActionResult(ok=True, message=message, data=data)


def install_cloudflare_token(runtime: Any, token_or_script: str) -> SetupActionResult:
    """Persist a Cloudflare tunnel token and mark remote access enabled."""
    token = extract_cloudflare_token(token_or_script)
    cf = runtime.config.remote_access.cloudflare
    token_path = Path(cf.token_path)
    try:
        token_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(str(token_path), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(token)
            fh.write("\n")
    except OSError as exc:
        return SetupActionResult(ok=False, message=f"Could not write token: {exc}")

    runtime.config.remote_access.provider = "cloudflare"
    cf.enabled = True
    saver = getattr(runtime.raw_runtime, "save_config", None)
    if callable(saver):
        try:
            saver()
        except Exception:
            pass

    data: dict[str, object] = {
        "token_path": str(token_path),
        "cloudflared_installed": bool(shutil.which("cloudflared")),
    }
    if shutil.which("systemctl"):
        data["service_command"] = f"sudo systemctl restart {cf.service_name}"
    return SetupActionResult(
        ok=True,
        message="Cloudflare tunnel token installed. Restart cloudflared to connect the tunnel.",
        data=data,
    )
