"""``build_setup_status`` orchestration.

Pulls together every input source (config, profile detection, hardware
check, persisted state, runtime managers) and composes the
``SetupStatus`` document the universal setup webapp consumes.
"""

from __future__ import annotations

from typing import Any

from ados import __version__
from ados.setup.hardware_check import run_hardware_check
from ados.setup.models import (
    MavlinkAccess,
    NetworkStatus,
    SetupStatus,
)
from ados.setup.profile import build_profile_suggestion
from ados.setup.state import read_state, record_ever_complete
from ados.setup.state_machine import _setup_steps

from ._access_urls import _access_urls, _mission_control_url, _video_access
from ._cloud_actions import _cloud_choice_status
from ._net_helpers import (
    _best_lan_host,
    _build_known_hosts,
    _first_mavlink_tcp_port,
    _first_mavlink_ws_port,
    _hostname,
    _local_ip_addresses,
    _local_ips,
    _probe_active_uplink_kind,
    _probe_wifi_rssi_dbm,
    _probe_wifi_ssid,
    _safe_host_for,
)
from ._service_inspection import _remote_status, _services


async def build_setup_status(  # noqa: C901
    runtime: Any, host_header: str | None = None,
) -> SetupStatus:
    """Build a complete setup status document from the API runtime facade."""
    config = runtime.config
    port = int(getattr(config.api.rest, "port", 8080))
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
    mavlink_tcp_port = _first_mavlink_tcp_port(config)

    # Pick a LAN-routable host for any URL a client elsewhere on the LAN
    # will dial. The agent's own webapp keeps using whatever the caller
    # sent in the Host header (typically localhost) so self-references
    # stay correct, but mavlink_tcp_url, mavlink_ws_url, and the LAN
    # video viewer all use the operator-friendly form here.
    lan_host = _best_lan_host(_hostname(), local_ips)
    if not lan_host:
        # Falls back to mDNS or host_name so URLs are never blank when
        # the agent has at least one identity to advertise.
        lan_host = mdns_host or host_name
    mavlink_url = f"ws://{lan_host}:{mavlink_ws_port}/"
    mavlink_tcp_url = (
        f"tcp://{lan_host}:{mavlink_tcp_port}" if mavlink_tcp_port else None
    )

    video = await _video_access(runtime, lan_host)
    remote = _remote_status(config)
    uplink_kind = _probe_active_uplink_kind()
    wifi_ssid = _probe_wifi_ssid() if uplink_kind == "wifi" else None
    rssi_dbm = _probe_wifi_rssi_dbm() if uplink_kind == "wifi" else None
    network = NetworkStatus(
        hostname=_hostname(),
        mdns_host=mdns_host,
        api_port=port,
        hotspot_enabled=bool(config.network.hotspot.enabled),
        hotspot_ssid=str(config.network.hotspot.ssid).replace(
            "{device_id}", config.agent.device_id or "device"
        ),
        local_ips=local_ips,
        uplink_kind=uplink_kind,
        wifi_ssid=wifi_ssid,
        rssi_dbm=rssi_dbm,
        ip_addresses=_local_ip_addresses(),
    )
    mavlink = MavlinkAccess(
        connected=fc.connected,
        port=str(fc.port or ""),
        baud=int(fc.baud or 0) if fc.baud is not None else None,
        websocket_url=mavlink_url,
        public_websocket_url=config.remote_access.cloudflare.mavlink_ws_url or None,
        tcp_url=mavlink_tcp_url,
    )
    if video.public_whep_url is None and config.remote_access.cloudflare.video_whep_url:
        video.public_whep_url = config.remote_access.cloudflare.video_whep_url

    services = _services(runtime)
    cloud_choice = _cloud_choice_status(config)
    profile_suggestion = build_profile_suggestion(config)
    profile_for_check = str(config.agent.profile)
    if profile_for_check == "auto":
        profile_for_check = profile_suggestion.detected
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
    reg_cfg = getattr(getattr(config, "network", None), "regulatory", None)
    reg_mode = str(getattr(reg_cfg, "mode", "unrestricted") or "unrestricted")
    reg_region = getattr(reg_cfg, "region", None)
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
        regulatory_mode=reg_mode,
        regulatory_region=reg_region,
    )

    # Apply persisted skip flags. Steps the operator chose to defer move
    # from `needs_action` to `optional` so they no longer block the
    # `setup_complete` derivation. We never downgrade `complete` or
    # `not_applicable` via skip.
    if persisted.skipped_steps:
        for step in steps:
            if step.id in persisted.skipped_steps and step.state == "needs_action":
                step.state = "optional"

    # Promote any currently-complete steps to the persisted "ever
    # complete" set so the percentage stays monotonic across transient
    # state flips (cloud relay reconnecting, FC heartbeat post-reboot,
    # video pipeline retry backoff, etc.). The percentage now answers
    # "how much of the install + configure flow is durably done" rather
    # than "how many runtime probes are passing right this instant".
    currently_complete = {step.id for step in steps if step.state == "complete"}
    persisted = record_ever_complete(currently_complete)

    countable = {
        step.id
        for step in steps
        if step.state == "complete" or step.id in persisted.ever_completed_steps
    }
    complete_steps = len(countable)
    completion_percent = round((complete_steps / len(steps)) * 100)
    next_step = next((step for step in steps if step.state == "needs_action"), None)
    next_action = (
        next_step.detail
        if next_step
        else "Open Mission Control or continue optional remote access setup"
    )

    natural_complete = not any(step.state == "needs_action" for step in steps)
    setup_complete = persisted.setup_finalized or natural_complete

    # After install the profile is always committed by `detect_profile`
    # (auto-detect) or by the operator (`apply_profile`). A live agent
    # is therefore always "configured" from the dashboard's point of
    # view; the operator can review or override via the webapp at any
    # time. `profile_source` carries the how so the GCS can show an
    # inline "auto-detected" hint without gating any UI.
    setup_state = "configured"
    explicit_profile = str(config.agent.profile) in ("drone", "ground_station")
    profile_source = (
        "user" if explicit_profile else profile_suggestion.source
    )

    # Pairing surface — read once, expose at the top level so the CLI
    # and webapp can render the code without walking the steps array.
    pairing_code: str | None = None
    paired_now = False
    pm = getattr(runtime.raw_runtime, "pairing_manager", None) or getattr(
        runtime, "pairing_manager", None
    )
    if pm is not None:
        try:
            paired_now = bool(getattr(pm, "is_paired", False))
            if not paired_now:
                pairing_code = pm.get_or_create_code()
        except Exception:
            pairing_code = None

    return SetupStatus(
        version=__version__,
        device_id=config.agent.device_id,
        device_name=config.agent.name,
        profile=config.agent.profile,
        ground_role=ground_role,
        setup_complete=setup_complete,
        setup_finalized=persisted.setup_finalized,
        setup_skipped=persisted.setup_skipped,
        setup_state=setup_state,  # type: ignore[arg-type]
        profile_source=profile_source,  # type: ignore[arg-type]
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
        pairing_code=pairing_code,
        paired=paired_now,
        lan_host=lan_host,
    )


__all__ = ["build_setup_status"]
