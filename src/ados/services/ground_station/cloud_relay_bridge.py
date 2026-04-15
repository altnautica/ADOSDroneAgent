"""Cloud relay bridge. Reconnects MQTT and Convex HTTP on uplink changes.

Wraps `services/mqtt/gateway.py` and `services/cloud/mavlink_relay.py`
and subscribes to `UplinkEventBus`. On every uplink change the bridge
tears the broker connection down and brings it back up so the kernel
routing table carries traffic over the new interface. On data-cap
threshold events the bridge downshifts what it forwards to the cloud:
at 95 percent it stops video forwarding and keeps telemetry, at 100
percent it drops everything except a minimal status heartbeat.

This module does not rewrite the MQTT gateway or the MAVLink relay.
It only orchestrates their lifecycle.

Per DEC-070, DEC-071, DEC-072. MSN-027 Wave B.
"""

from __future__ import annotations

import asyncio
import json
import time
from typing import Any, Optional

import structlog

from ados.services.ground_station.uplink_router import (
    UplinkEvent,
    UplinkEventBus,
)

log = structlog.get_logger("ground_station.cloud_relay_bridge")


# ----------------------------------------------------------------------
# Tunables
# ----------------------------------------------------------------------
_RECONNECT_BASE_SECONDS = 2.0
_RECONNECT_MAX_SECONDS = 300.0
_RECONNECT_MULTIPLIER = 2.0
_UPLINK_SETTLE_SECONDS = 2.0
_STATUS_HEARTBEAT_INTERVAL = 30.0
_CONVEX_POST_TIMEOUT_SECONDS = 8.0

# Throttle states mirror `uplink_router.DataCapState` string values.
_THROTTLE_NONE = "none"
_THROTTLE_WARN = "warn_80"
_THROTTLE_VIDEO_OFF = "throttle_95"
_THROTTLE_BLOCKED = "blocked_100"


class CloudRelayBridge:
    """Orchestrates MQTT plus Convex HTTP against the current uplink.

    The bridge owns a lifecycle loop that listens to the uplink bus
    and reconciles the MQTT gateway and the MAVLink relay on every
    relevant event. When no uplink is active the bridge sits idle.
    """

    def __init__(
        self,
        uplink_bus: UplinkEventBus,
        mqtt_gateway: Any,
        mavlink_relay: Any,
        paired_drone_id: Optional[str],
        convex_base_url: str = "https://convex.altnautica.com",
        api_key: Optional[str] = None,
        state_reader: Any = None,
    ) -> None:
        self._bus = uplink_bus
        self._mqtt = mqtt_gateway
        self._relay = mavlink_relay
        self._drone_id = paired_drone_id
        self._convex_base = convex_base_url.rstrip("/")
        self._api_key = api_key
        # Phase 4 Wave 2 Cellos: optional StateReader. When wired the
        # bridge owns its lifecycle so MqttGateway and the Convex
        # heartbeat read live VehicleState rather than the stub.
        self._state_reader = state_reader

        self._running = False
        self._main_task: Optional[asyncio.Task] = None
        self._mqtt_task: Optional[asyncio.Task] = None
        self._relay_task: Optional[asyncio.Task] = None
        self._heartbeat_task: Optional[asyncio.Task] = None
        self._mqtt_shutdown: Optional[asyncio.Event] = None
        self._relay_shutdown: Optional[asyncio.Event] = None

        self._throttle_state: str = _THROTTLE_NONE
        self._forward_video: bool = True
        self._forward_telemetry: bool = True
        self._mqtt_connected: bool = False
        self._last_mqtt_ok: Optional[float] = None
        self._last_convex_ok: Optional[float] = None
        self._reconnect_attempts: int = 0
        self._reconnect_delay: float = _RECONNECT_BASE_SECONDS
        self._current_uplink: Optional[str] = None

    # ------------------------------------------------------------------
    # Public lifecycle
    # ------------------------------------------------------------------
    async def start(self) -> None:
        if self._running:
            return
        self._running = True
        # Phase 4 Wave 2 Cellos: bring up the state reader before the
        # MQTT and heartbeat tasks so first publish carries fresh data.
        if self._state_reader is not None:
            try:
                await self._state_reader.start()
            except Exception as exc:
                log.warning("cloud_relay.state_reader_start_failed", error=str(exc))
        self._main_task = asyncio.create_task(self._run())
        self._heartbeat_task = asyncio.create_task(self._convex_heartbeat_loop())
        log.info(
            "cloud_relay.start",
            drone_id=self._drone_id,
            convex_base=self._convex_base,
        )

    async def stop(self) -> None:
        if not self._running:
            return
        self._running = False
        await self._teardown_mqtt()
        for task in (self._main_task, self._heartbeat_task):
            if task is not None:
                task.cancel()
        for task in (self._main_task, self._heartbeat_task):
            if task is not None:
                try:
                    await task
                except (asyncio.CancelledError, Exception):
                    pass
        self._main_task = None
        self._heartbeat_task = None
        if self._state_reader is not None:
            try:
                await self._state_reader.stop()
            except Exception as exc:
                log.debug("cloud_relay.state_reader_stop_failed", error=str(exc))
        log.info("cloud_relay.stop")

    def status(self) -> dict:
        return {
            "mqtt_connected": self._mqtt_connected,
            "last_mqtt_ok": self._last_mqtt_ok,
            "last_convex_ok": self._last_convex_ok,
            "throttle_state": self._throttle_state,
            "forwarding_video_to_cloud": self._forward_video,
            "forwarding_telemetry_to_cloud": self._forward_telemetry,
            "reconnect_attempts": self._reconnect_attempts,
            "current_uplink": self._current_uplink,
            "paired_drone_id": self._drone_id,
        }

    # ------------------------------------------------------------------
    # Main event loop
    # ------------------------------------------------------------------
    async def _run(self) -> None:
        log.info("cloud_relay.loop_start")
        try:
            async for evt in self._bus.subscribe():
                if not self._running:
                    return
                try:
                    await self._handle_event(evt)
                except Exception as exc:
                    log.warning(
                        "cloud_relay.handle_event_failed",
                        kind=evt.kind,
                        error=str(exc),
                    )
        except asyncio.CancelledError:
            pass
        except Exception as exc:
            log.error("cloud_relay.loop_error", error=str(exc))
        finally:
            log.info("cloud_relay.loop_stop")

    async def _handle_event(self, evt: UplinkEvent) -> None:
        if evt.kind == "uplink_changed":
            await self._on_uplink_changed(evt)
        elif evt.kind == "health_changed":
            await self._on_health_changed(evt)
        elif evt.kind == "data_cap_threshold":
            await self._on_data_cap_threshold(evt)

    async def _on_uplink_changed(self, evt: UplinkEvent) -> None:
        new_uplink = evt.active_uplink
        log.info(
            "cloud_relay.uplink_changed",
            previous=self._current_uplink,
            new=new_uplink,
            reachable=evt.internet_reachable,
        )
        self._current_uplink = new_uplink

        await self._teardown_mqtt()

        if not new_uplink or not evt.internet_reachable:
            log.info("cloud_relay.no_uplink_idle")
            return

        await asyncio.sleep(_UPLINK_SETTLE_SECONDS)
        await self._bring_up_mqtt_with_retry()

    async def _on_health_changed(self, evt: UplinkEvent) -> None:
        if evt.internet_reachable and not self._mqtt_connected:
            log.info("cloud_relay.health_restored_reconnect")
            await self._bring_up_mqtt_with_retry()
        elif not evt.internet_reachable and self._mqtt_connected:
            log.info("cloud_relay.health_lost_teardown")
            await self._teardown_mqtt()

    async def _on_data_cap_threshold(self, evt: UplinkEvent) -> None:
        state = evt.data_cap_state or _THROTTLE_NONE
        previous = self._throttle_state
        self._throttle_state = str(state)
        if state == _THROTTLE_BLOCKED:
            self._forward_video = False
            self._forward_telemetry = False
            log.warning(
                "cloud_relay.data_cap_blocked",
                previous=previous,
                state=state,
                note="heartbeat_only",
            )
            await self._teardown_relay()
        elif state == _THROTTLE_VIDEO_OFF:
            self._forward_video = False
            self._forward_telemetry = True
            log.warning(
                "cloud_relay.data_cap_throttle",
                previous=previous,
                state=state,
                note="video_forwarding_stopped",
            )
        else:
            self._forward_video = True
            self._forward_telemetry = True
            log.info(
                "cloud_relay.data_cap_ok",
                previous=previous,
                state=state,
            )
            if self._mqtt_connected and self._relay_task is None:
                await self._bring_up_relay()

    # ------------------------------------------------------------------
    # MQTT + relay lifecycle
    # ------------------------------------------------------------------
    async def _bring_up_mqtt_with_retry(self) -> None:
        self._reconnect_delay = _RECONNECT_BASE_SECONDS
        while self._running:
            try:
                await self._bring_up_mqtt()
                self._reconnect_attempts = 0
                self._reconnect_delay = _RECONNECT_BASE_SECONDS
                return
            except Exception as exc:
                self._reconnect_attempts += 1
                log.warning(
                    "cloud_relay.mqtt_connect_failed",
                    attempt=self._reconnect_attempts,
                    delay_s=self._reconnect_delay,
                    error=str(exc),
                )
                try:
                    await asyncio.sleep(self._reconnect_delay)
                except asyncio.CancelledError:
                    return
                self._reconnect_delay = min(
                    self._reconnect_delay * _RECONNECT_MULTIPLIER,
                    _RECONNECT_MAX_SECONDS,
                )

    async def _bring_up_mqtt(self) -> None:
        if self._mqtt is None:
            log.info("cloud_relay.mqtt_gateway_missing")
            return
        if self._mqtt_task is not None and not self._mqtt_task.done():
            return
        self._mqtt_shutdown = asyncio.Event()
        self._mqtt_task = asyncio.create_task(
            self._mqtt.run(self._mqtt_shutdown)
        )
        await asyncio.sleep(1.0)
        if self._mqtt_task.done():
            exc = self._mqtt_task.exception()
            if exc is not None:
                raise exc
            raise RuntimeError("mqtt gateway exited immediately")
        self._mqtt_connected = True
        self._last_mqtt_ok = time.time()
        log.info("cloud_relay.mqtt_connected")
        if self._forward_telemetry and self._throttle_state != _THROTTLE_BLOCKED:
            await self._bring_up_relay()

    async def _bring_up_relay(self) -> None:
        if self._relay is None:
            return
        if self._relay_task is not None and not self._relay_task.done():
            return
        self._relay_shutdown = asyncio.Event()
        self._relay_task = asyncio.create_task(
            self._relay.start(self._relay_shutdown)
        )
        log.info("cloud_relay.mavlink_relay_started")

    async def _teardown_relay(self) -> None:
        if self._relay_shutdown is not None:
            self._relay_shutdown.set()
        if self._relay_task is not None:
            try:
                await asyncio.wait_for(self._relay_task, timeout=5.0)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                self._relay_task.cancel()
            except Exception as exc:
                log.debug("cloud_relay.relay_stop_error", error=str(exc))
            self._relay_task = None
        self._relay_shutdown = None

    async def _teardown_mqtt(self) -> None:
        await self._teardown_relay()
        if self._mqtt_shutdown is not None:
            self._mqtt_shutdown.set()
        if self._mqtt_task is not None:
            try:
                await asyncio.wait_for(self._mqtt_task, timeout=5.0)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                self._mqtt_task.cancel()
            except Exception as exc:
                log.debug("cloud_relay.mqtt_stop_error", error=str(exc))
            self._mqtt_task = None
        self._mqtt_shutdown = None
        self._mqtt_connected = False

    # ------------------------------------------------------------------
    # Convex heartbeat
    # ------------------------------------------------------------------
    async def _convex_heartbeat_loop(self) -> None:
        """Post a minimal status beacon to Convex every 30 seconds.

        Runs even when the MQTT gateway is down so the cloud has a
        low-cost presence signal. Skips posting when there is no
        uplink at all.
        """
        try:
            import httpx
        except ImportError:
            log.info("cloud_relay.httpx_unavailable")
            httpx = None  # type: ignore[assignment]

        while self._running:
            try:
                await asyncio.sleep(_STATUS_HEARTBEAT_INTERVAL)
                if httpx is None:
                    continue
                if not self._current_uplink:
                    continue
                payload = {
                    "drone_id": self._drone_id,
                    "uplink": self._current_uplink,
                    "mqtt_connected": self._mqtt_connected,
                    "throttle_state": self._throttle_state,
                    "forwarding_video": self._forward_video,
                    "forwarding_telemetry": self._forward_telemetry,
                    "ts_ms": int(time.time() * 1000),
                }
                # Phase 4 Wave 2 Cellos: enrich the heartbeat with live
                # VehicleState (armed, mode, lat/lon, battery) when a
                # state reader is wired. Falls back silently when the
                # reader is absent or has not yet received its first
                # snapshot.
                if self._state_reader is not None:
                    try:
                        vs = self._state_reader.get_latest()
                        payload["telemetry"] = {
                            "armed": bool(vs.armed),
                            "mode": vs.mode,
                            "lat": vs.lat,
                            "lon": vs.lon,
                            "alt_rel": vs.alt_rel,
                            "battery_voltage": vs.voltage_battery,
                            "battery_remaining": vs.battery_remaining,
                            "fc_connected": bool(vs.last_heartbeat),
                            "last_heartbeat": vs.last_heartbeat,
                        }
                    except Exception as exc:
                        log.debug(
                            "cloud_relay.heartbeat_telemetry_failed",
                            error=str(exc),
                        )
                headers: dict[str, str] = {"content-type": "application/json"}
                if self._api_key:
                    headers["x-ados-key"] = self._api_key
                url = f"{self._convex_base}/agent/status"
                async with httpx.AsyncClient(
                    timeout=_CONVEX_POST_TIMEOUT_SECONDS
                ) as client:
                    resp = await client.post(
                        url,
                        content=json.dumps(payload),
                        headers=headers,
                    )
                if 200 <= resp.status_code < 300:
                    self._last_convex_ok = time.time()
                else:
                    log.debug(
                        "cloud_relay.convex_post_non2xx",
                        status=resp.status_code,
                    )
            except asyncio.CancelledError:
                return
            except Exception as exc:
                log.debug("cloud_relay.convex_post_failed", error=str(exc))


# ----------------------------------------------------------------------
# Module-level singleton
# ----------------------------------------------------------------------
_instance: "CloudRelayBridge | None" = None


def get_cloud_relay_bridge() -> "CloudRelayBridge":
    """Return (and lazily construct) the singleton bridge.

    Lazy construction pulls in the uplink router, MQTT gateway,
    MAVLink relay, and pair manager. Any import or init failure
    falls back to a bridge wired with None collaborators so
    `status()` still works and the service does not crash on boot.
    """
    global _instance
    if _instance is None:
        _instance = _build_default_bridge()
    return _instance


def _build_default_bridge() -> "CloudRelayBridge":
    try:
        from ados.core.config import load_config
        from ados.services.ground_station.uplink_router import get_uplink_router
        from ados.services.mqtt.gateway import MqttGateway
        from ados.services.cloud.mavlink_relay import MavlinkMqttRelay
        from ados.services.mavlink.state import VehicleState
        from ados.services.ground_station.pair_manager import get_pair_manager
    except Exception as exc:
        log.warning("cloud_relay.dependency_import_failed", error=str(exc))
        return CloudRelayBridge(
            uplink_bus=UplinkEventBus(),
            mqtt_gateway=None,
            mavlink_relay=None,
            paired_drone_id=None,
        )

    try:
        config = load_config()
        router = get_uplink_router()
        pair = get_pair_manager()
        drone_id: Optional[str] = None
        # Wave B blocker 2 fix: read paired_drone_id from PairManager.status()
        # rather than config.ground_station.paired_drone_id. The pair manager
        # is the authoritative source. `status()` is async so we run it
        # synchronously here via asyncio.run when no loop is active, or fall
        # back to the config field if a loop is already running (systemd boot
        # path with an embedded event loop).
        try:
            try:
                _running_loop = asyncio.get_running_loop()
            except RuntimeError:
                _running_loop = None
            if _running_loop is None:
                pair_status = asyncio.run(pair.status())
            else:
                pair_status = {}
                log.debug(
                    "cloud_relay.pair_status_skipped_running_loop",
                    note="deferring to async status refresh at runtime",
                )
            if isinstance(pair_status, dict) and pair_status.get("paired"):
                drone_id = pair_status.get("paired_drone_id") or None
            if not drone_id:
                gs = getattr(config, "ground_station", None)
                if gs is not None:
                    drone_id = getattr(gs, "paired_drone_id", None) or None
        except Exception as exc:
            log.debug("cloud_relay.pair_status_failed", error=str(exc))
            try:
                gs = getattr(config, "ground_station", None)
                if gs is not None:
                    drone_id = getattr(gs, "paired_drone_id", None) or None
            except Exception:
                drone_id = None

        # Phase 4 Wave 2 Cellos (Wave B blocker 1 fix): wire live
        # VehicleState via the ados-mavlink state IPC socket
        # (`/run/ados/state.sock`). Bridge owns the StateReader. The
        # MqttGateway and Convex heartbeat path read the same
        # VehicleState object so both publish fresh telemetry.
        # Gated by `ground_station.use_live_state_ipc` for rollback.
        state_reader = None
        use_live_ipc = bool(
            getattr(getattr(config, "ground_station", None), "use_live_state_ipc", True)
        )
        if use_live_ipc:
            try:
                from ados.services.ground_station.state_reader import StateReader
                state_reader = StateReader()
                state = state_reader.vehicle_state
            except Exception as exc:
                log.warning("cloud_relay.state_reader_init_failed", error=str(exc))
                state_reader = None
                state = VehicleState()
        else:
            log.info("cloud_relay.state_ipc_disabled_by_config")
            state = VehicleState()
        api_key = getattr(config.security.api, "api_key", None) or None
        mqtt_gw = MqttGateway(config, state, api_key=api_key)

        if config.server.mode == "self_hosted":
            broker = config.server.self_hosted.mqtt_broker
            port = config.server.self_hosted.mqtt_port
        else:
            broker = config.server.cloud.mqtt_broker
            port = config.server.cloud.mqtt_port
        transport = config.server.mqtt_transport

        relay_username = ""
        relay_password = ""
        if config.server.mode == "cloud" and api_key:
            relay_username = f"ados-{config.agent.device_id}"
            relay_password = api_key

        relay = MavlinkMqttRelay(
            device_id=config.agent.device_id,
            broker=broker,
            port=port,
            transport=transport,
            username=relay_username,
            password=relay_password,
        )

        return CloudRelayBridge(
            uplink_bus=router.bus,
            mqtt_gateway=mqtt_gw,
            mavlink_relay=relay,
            paired_drone_id=drone_id,
            api_key=api_key,
            state_reader=state_reader,
        )
    except Exception as exc:
        log.warning("cloud_relay.build_failed", error=str(exc))
        return CloudRelayBridge(
            uplink_bus=UplinkEventBus(),
            mqtt_gateway=None,
            mavlink_relay=None,
            paired_drone_id=None,
        )


# ----------------------------------------------------------------------
# Service entry point (placeholder for Wave C systemd unit)
# ----------------------------------------------------------------------
async def _run_service() -> None:
    """Run the cloud relay bridge as a standalone systemd service.

    Wave B blocker 3 fix: this entry MUST NOT start the uplink router.
    The router is owned by `ados-uplink-router.service`. The bridge is
    a subscriber to the router's UplinkEventBus. If the router has not
    yet come up when the bridge starts, `bus.subscribe()` still works
    (queue fills when the router eventually publishes). We log a
    warning so the operator sees the dependency gap in journalctl.
    """
    import signal

    bridge = get_cloud_relay_bridge()

    # Warn if the router is clearly not running yet. This is a soft
    # check: we do not block, we do not attempt to start the router,
    # we just surface the fact that the bus is empty so the paired
    # systemd After=/Requires= ordering can be diagnosed.
    bus = getattr(bridge, "_bus", None)
    if bus is None or not getattr(bus, "_subscribers", None):
        log.warning(
            "cloud_relay.router_not_observed",
            note="ados-uplink-router not yet publishing; will catch up when it starts",
        )

    await bridge.start()

    stop_event = asyncio.Event()
    loop = asyncio.get_running_loop()

    def _handle_signal(signame: str) -> None:
        log.info("cloud_relay.signal_received", signal=signame)
        stop_event.set()

    for signame in ("SIGINT", "SIGTERM"):
        try:
            loop.add_signal_handler(
                getattr(signal, signame), _handle_signal, signame
            )
        except NotImplementedError:
            pass

    log.info("cloud_relay.service_ready", status=bridge.status())
    await stop_event.wait()

    await bridge.stop()
    log.info("cloud_relay.service_stopped")


def main() -> None:
    """Systemd entry point for the future `ados-cloud-relay.service`."""
    try:
        asyncio.run(_run_service())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
