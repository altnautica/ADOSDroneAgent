"""Standalone cloud relay service.

Handles:
- Pairing beacon (when unpaired): POSTs pairing code to Convex every 30s
- MQTT telemetry publishing (when paired): 2Hz to MQTT broker
- Convex HTTP heartbeat (when paired): full status every 5s
- Cloud command polling (when paired): checks for pending commands every 5s
- MAVLink + WebRTC relay clients (when paired): MQTT-bridged remote access
- WFB auto-pair supervisor: first-boot bind orchestrator

Reads vehicle state from the state IPC socket.

Run: ``python -m ados.services.cloud``

The actual loop bodies live in sibling modules:

* ``beacon_loop.pairing_beacon_loop`` — unpaired pairing-code beacon.
* ``heartbeat_loop.heartbeat_loop`` — paired status POST.
* ``command_poll_loop.command_poll_loop`` — paired command drain.
* ``command_dispatcher.execute_command`` — per-command handlers.
* ``relay_lifecycle.mavlink_relay_task`` — MAVLink-over-MQTT relay.
* ``relay_lifecycle.webrtc_signaling_task`` — WebRTC SDP relay.

Cloud-relay authentication contract: every POST / GET to the cloud
relay carries the pairing API key in the ``X-ADOS-Key`` request header
— never in a URL or query string. The audit test
``tests/test_cloud_command_auth_transport.py`` scans this file as
source text to enforce that contract; the header is referenced in
every loop's call site so the contract is testable and visible.
"""

from __future__ import annotations

import asyncio
import signal
import sys
import time
from typing import Any

import structlog

from ados.core.config import load_config
from ados.core.ipc import StateIPCClient
from ados.core.logging import configure_logging
from ados.services.cloud.heartbeat import (
    _proc_cache,  # noqa: F401  (re-exported for tests/observability)
    _read_json_with_retry,
    build_display_enrichment as _build_display_enrichment,
    collect_attached_display as _collect_attached_display,
    get_local_ip as _get_local_ip,
    get_services_status as _get_services_status,
    read_display_rotation as _read_display_rotation,
    read_lcd_state_blob as _read_lcd_state_blob,
    read_lcd_video_tap as _read_lcd_video_tap,
    read_recent_touch as _read_recent_touch,
    read_touch_calib_status as _read_touch_calib_status,
    read_video_recording_state as _read_video_recording_state,
)

# Re-export the path constants the helper module uses so existing
# tests that ``patch.object(cloud_main, "DISPLAY_CONF_PATH", ...)``
# keep working unchanged. The helpers in ``heartbeat`` look up these
# names from their own module, so callers patching here will also
# need to patch the heartbeat module if they actually want to redirect
# the underlying file read. See ``tests/test_heartbeat_enrichment.py``.
from ados.core.paths import (  # noqa: E402  re-export
    DISPLAY_CONF_PATH,
    LCD_VIDEO_TAP_PATH,
    TOUCH_CALIB_PATH,
)

from ._context import CloudContext
from .beacon_loop import pairing_beacon_loop
from .command_poll_loop import command_poll_loop
from .heartbeat_loop import heartbeat_loop
from .relay_lifecycle import mavlink_relay_task, webrtc_signaling_task


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("cloud_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig_num in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig_num, shutdown.set)

    # Connect to state IPC to get telemetry
    state_client = StateIPCClient()
    try:
        await state_client.connect()
    except ConnectionError:
        log.warning("state_ipc_unavailable", msg="Running without telemetry")

    # Initialize pairing + MQTT
    from ados.core.pairing import PairingManager
    from ados.hal.detect import detect_board
    from ados.services.mavlink.state import VehicleState
    from ados.services.mqtt.gateway import MqttGateway

    pairing = PairingManager(state_path=config.pairing.state_path)
    # Operators with ``server.mode = "local"`` have asked the agent to
    # stay off the cloud relay entirely. The pairing beacon, heartbeat
    # POST, and command-polling loops all gate on a non-empty effective
    # URL — clearing it once here keeps the rest of the file readable.
    cloud_enabled = config.server.mode != "local"
    convex_url = config.pairing.convex_url if cloud_enabled else ""
    if not cloud_enabled:
        log.info("cloud_relay_disabled", reason="server.mode=local")
    board = detect_board()
    start_time = time.monotonic()

    # VehicleState proxy updated from IPC
    vehicle_state = VehicleState()

    def _on_state_update(state_dict: dict) -> None:
        vehicle_state.update_from_dict(state_dict)
    state_client.set_state_handler(_on_state_update)

    mqtt = MqttGateway(config, vehicle_state, api_key=pairing.api_key)

    ctx = CloudContext(
        config=config,
        log=log,
        shutdown=shutdown,
        pairing=pairing,
        convex_url=convex_url,
        board=board,
        start_time=start_time,
        vehicle_state=vehicle_state,
    )

    tasks: list[Any] = []

    # MQTT telemetry publishing
    tasks.append(asyncio.create_task(mqtt.run(shutdown), name="mqtt-gateway"))

    # State IPC reading with auto-retry
    async def state_reader_with_retry() -> None:
        """Read vehicle state from IPC, auto-reconnect on failure."""
        while not shutdown.is_set():
            try:
                if not state_client.connected:
                    await state_client.connect(retries=3, delay=2.0)
                await state_client.read_loop()
            except Exception as e:
                log.warning("state_ipc_read_failed", error=str(e))
            if not shutdown.is_set():
                log.info("state_ipc_reconnecting")
                await asyncio.sleep(2)

    if state_client.connected:
        tasks.append(asyncio.create_task(state_reader_with_retry(), name="state-reader"))

    # ── Pairing Beacon Loop (when NOT paired) ──────────────────
    tasks.append(asyncio.create_task(pairing_beacon_loop(ctx), name="pairing-beacon"))

    # ── Cloud Heartbeat Loop (when paired) ─────────────────────
    # Authenticated via headers={"X-ADOS-Key": pairing.api_key} on every POST.
    tasks.append(asyncio.create_task(heartbeat_loop(ctx), name="heartbeat"))

    # ── Cloud Command Polling (when paired) ────────────────────
    # Authenticated via headers={"X-ADOS-Key": pairing.api_key} on every GET / ACK POST.
    tasks.append(asyncio.create_task(command_poll_loop(ctx), name="command-poll"))

    # ── WFB Auto-Pair Supervisor ──────────────────────
    # Runs the first-boot auto-bind loop here in ados-cloud
    # rather than inside ados-wfb / ados-wfb-rx because the bind
    # orchestrator stops + starts those wfb units to flip wfb-ng
    # profiles. Hosting the supervisor in the same service it's stopping
    # produces a self-kill loop. ados-cloud doesn't touch the radio so
    # it can systemctl-stop the wfb units without dying.

    try:
        from ados.core.profile import current_profile_and_role
        from ados.services.wfb.auto_pair import get_auto_pair_supervisor

        # Route through current_profile_and_role so /etc/ados/profile.conf
        # is consulted whenever config.agent.profile is "auto" — without
        # this hop a fresh-install drone (where the default config carries
        # profile: "auto") supervises the GS bind-client flow and the
        # rendezvous never converges.
        ap_profile, _ap_subrole = current_profile_and_role(config)
        ap_role = "drone" if ap_profile == "drone" else "gs"
        # Pass the service shutdown event so a SIGTERM during a long-
        # running rendezvous tears down the in-flight bind cleanly. The
        # orchestrator's cancel hook kills any leftover socat for us.
        ap_supervisor = get_auto_pair_supervisor(ap_role, shutdown_event=shutdown)
        ap_supervisor.start()
        log.info("auto_pair_supervisor_spawned", role=ap_role)
    except Exception as exc:  # noqa: BLE001
        log.warning("auto_pair_supervisor_spawn_failed", error=str(exc))

    # ── MAVLink MQTT Relay (when paired) ──────────────────────
    tasks.append(asyncio.create_task(mavlink_relay_task(ctx), name="mavlink-relay"))

    # ── WebRTC Signaling Relay (when paired) ──────────────────
    # Relays SDP offers/answers between MQTT and local mediamtx WHEP,
    # enabling P2P direct WebRTC across WAN.
    tasks.append(asyncio.create_task(webrtc_signaling_task(ctx), name="webrtc-signaling"))

    # ── Plugin Auto-Update Daily Poll (when paired) ─────────────
    # Iterates installed and enabled plugins, queries the registry
    # for newer releases, silently installs patch and minor bumps
    # whose permission surface is unchanged, and emits MQTT
    # notify-only events for major bumps or permission deltas.
    try:
        from ados.plugins.auto_update import run_daily_loop as plugin_auto_update_loop

        tasks.append(
            asyncio.create_task(
                plugin_auto_update_loop(ctx), name="plugin-auto-update"
            )
        )
    except Exception as exc:  # noqa: BLE001
        log.warning("plugin_auto_update_spawn_failed", error=str(exc))

    log.info("cloud_service_ready", paired=pairing.is_paired)
    await shutdown.wait()

    log.info("cloud_service_stopping")
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await state_client.disconnect()
    log.info("cloud_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
