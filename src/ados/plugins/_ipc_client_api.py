"""Request/response API methods of :class:`ados.plugins.ipc_client.PluginIpcClient`."""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator, Awaitable, Callable
from typing import Any

from ados.plugins._dispatch_generated import REQUIRED_CAP
from ados.plugins._ipc_client_common import (
    TELEMETRY_STATE_TOPIC,
    _driver_kind_capability,
    _IpcClientState,
)


class _PluginApiMixin(_IpcClientState):
    """The plugin-facing request methods; each wraps one ``_send_request``."""

    async def ping(self) -> dict:
        return (await self._send_request("ping", capability="", args={})).args

    async def event_publish(self, topic: str, payload: dict) -> int:
        result = await self._send_request(
            "event.publish",
            capability="event.publish",
            args={"topic": topic, "payload": payload},
        )
        return int(result.args.get("delivered", 0))

    async def event_subscribe(
        self,
        topic_pattern: str,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        self._event_callbacks.setdefault(topic_pattern, []).append(callback)
        await self._send_request(
            "event.subscribe",
            capability="event.subscribe",
            args={"topic": topic_pattern},
        )

    # ---- Telemetry ----------------------------------------------------

    async def subscribe_telemetry(
        self, topic: str = TELEMETRY_STATE_TOPIC
    ) -> AsyncIterator[dict]:
        """Yield the vehicle-state snapshot as the host pushes it.

        Same wire as the Rust SDK's ``telemetry_subscribe``: a
        ``telemetry.subscribe`` request gated on ``telemetry.read``, after
        which the host pushes the state object as an event on
        ``telemetry.state``, rate-limited and always the newest. Each yielded
        item is that state object. A consumer slower than the push sees the
        newest snapshot, never a backlog. A node with no flight controller
        never yields.
        """
        if topic != TELEMETRY_STATE_TOPIC:
            raise ValueError(f"the host pushes telemetry on {TELEMETRY_STATE_TOPIC!r} only")
        latest: asyncio.Queue[dict] = asyncio.Queue(maxsize=1)

        def _push(payload: dict) -> None:
            if latest.full():
                latest.get_nowait()
            latest.put_nowait(payload)

        callbacks = self._event_callbacks.setdefault(topic, [])
        callbacks.append(_push)
        try:
            await self._send_request(
                "telemetry.subscribe",
                capability=REQUIRED_CAP["telemetry.subscribe"],
                args={},
            )
            while True:
                yield await latest.get()
        finally:
            callbacks.remove(_push)

    # ---- MAVLink ------------------------------------------------------

    async def mavlink_send(
        self, msg_bytes: bytes, component_id: int | None = None
    ) -> dict:
        args: dict[str, Any] = {"msg_bytes": bytes(msg_bytes)}
        if component_id is not None:
            args["component_id"] = int(component_id)
        return (
            await self._send_request(
                "mavlink.send", capability="mavlink.write", args=args
            )
        ).args

    async def mavlink_register_component(self, comp_id: int, kind: str) -> dict:
        return (
            await self._send_request(
                "mavlink.register_component",
                capability=f"mavlink.component.{kind}",
                args={"component_id": int(comp_id), "kind": kind},
            )
        ).args

    async def mavlink_subscribe(
        self,
        msg_name: str,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        # MAVLink deliveries arrive as ``event``-type envelopes routed
        # through the same reader loop as event bus deliveries; the
        # dispatcher routes them by topic.
        self._mavlink_callbacks.setdefault(msg_name, []).append(callback)
        await self._send_request(
            "mavlink.subscribe",
            capability="mavlink.read",
            args={"msg_name": msg_name},
        )

    # ---- MSP (Betaflight / iNav / KISS byte plane) --------------------

    async def msp_send(self, msg_bytes: bytes) -> dict:
        """Write already-framed MSP bytes (built with ``ados_protocol::msp`` /
        the plugin's own codec) to the FC. Gated on ``msp.write``."""
        return (
            await self._send_request(
                "msp.send",
                capability="msp.write",
                args={"msg_bytes": bytes(msg_bytes)},
            )
        ).args

    async def msp_subscribe(
        self,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        """Subscribe to the raw FC->host MSP byte stream. Deliveries arrive as
        ``msp.deliver`` events carrying ``{bytes, timestamp_ms}``; MSP has no
        per-message topic, so the callback sees every chunk. Gated on
        ``msp.read``."""
        self._msp_callbacks.append(callback)
        await self._send_request(
            "msp.subscribe",
            capability="msp.read",
            args={},
        )

    # ---- Vision -------------------------------------------------------

    async def vision_subscribe_detections(
        self,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        """Register a callback for ``vision.deliver_detection`` events.

        The subscribe RPC is sent by the SDK facade; this only records the local
        deliver callback. Each delivered batch is routed here by method name (the
        event carries no ``topic``) and the callback receives ``{batch,
        timestamp_ms}``.
        """
        self._detection_callbacks.append(callback)

    async def vision_read_model(self) -> list[dict]:
        """Read this plugin's resolved model-delivery status.

        Returns one dict per declared model — ``{state, model_id, runtime, path,
        reason}`` (the resolver's ``ModelResolution.to_dict()``), keyed by
        ``model_id`` — so the plugin can find where its delivered model was
        cached (``path`` when ``state == "resolved"``). An unresolved plugin
        returns an empty list rather than raising, so a caller can poll.
        """
        resp = await self._send_request(
            "vision.read_model",
            capability="vision.model.read",
            args={},
        )
        return (resp.args or {}).get("models", [])

    def register_button_callback(
        self,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        """Register a callback for ``button.deliver`` events.

        Like the detection stream, the event carries no ``topic`` and is routed
        by method name, so it needs its own callback list rather than riding the
        generic topic dispatch. The callback receives the press directly:
        ``{pin, kind, action, timestamp_ms}``, where ``action`` is ``None`` for an
        unmapped button.
        """
        self._button_callbacks.append(callback)

    async def button_subscribe(self) -> dict:
        """Arm the host's front-panel button push stream for this connection."""
        return await self._send_request(
            "button.subscribe",
            capability="button.subscribe",
            args={},
        )

    # ---- Telemetry ----------------------------------------------------

    async def telemetry_extend(self, channel: str, payload: dict) -> dict:
        return (
            await self._send_request(
                "telemetry.extend",
                capability="telemetry.extend",
                args={"channel": channel, "payload": payload},
            )
        ).args

    # ---- Peripheral manager ------------------------------------------

    async def peripheral_register_driver(self, kind: str, driver_ref: str) -> dict:
        capability = _driver_kind_capability(kind)
        return (
            await self._send_request(
                "peripheral.register_driver",
                capability=capability,
                args={"kind": kind, "driver_ref": driver_ref},
            )
        ).args

    async def peripheral_unregister_driver(self, handle_id: str) -> dict:
        return (
            await self._send_request(
                "peripheral.unregister_driver",
                capability="",
                args={"handle_id": handle_id},
            )
        ).args

    async def camera_claim(self, device_path: str, exclusive: bool = True) -> dict:
        return (
            await self._send_request(
                "camera.claim",
                capability="sensor.camera.register",
                args={"device_path": device_path, "exclusive": exclusive},
            )
        ).args

    async def camera_release(self, device_path: str) -> dict:
        return (
            await self._send_request(
                "camera.release",
                capability="sensor.camera.register",
                args={"device_path": device_path},
            )
        ).args

    async def camera_get_frame(
        self,
        device_path: str,
        *,
        format: str = "nv12",
        timeout_ms: int = 1000,
    ) -> dict:
        return (
            await self._send_request(
                "camera.get_frame",
                capability="sensor.camera.register",
                args={
                    "device_path": device_path,
                    "format": format,
                    "timeout_ms": int(timeout_ms),
                },
            )
        ).args

    # ---- Video source -------------------------------------------------

    async def video_source_set(self, cameras: list[dict]) -> dict:
        """Declare the video pipeline's stream sources. The host forwards the
        list to the supervisor, which persists ``video.cameras`` and restarts
        the pipeline. Gated on the ``video.source.set`` capability."""
        return (
            await self._send_request(
                "video.source.set",
                capability="video.source.set",
                args={"cameras": list(cameras)},
            )
        ).args

    # ---- Flight setpoints ---------------------------------------------

    async def flight_guided_setpoint_send(self, setpoint: dict) -> dict:
        """Send a guided-mode position/velocity setpoint to the flight
        controller. The host forwards it to the MAVLink router, which encodes
        the appropriate SET_POSITION_TARGET message. Gated on the
        ``flight.guided_setpoint`` capability."""
        return (
            await self._send_request(
                "flight.guided_setpoint.send",
                capability="flight.guided_setpoint",
                args=dict(setpoint),
            )
        ).args

    async def flight_rate_setpoint_send(self, setpoint: dict) -> dict:
        """Send an attitude/body-rate + thrust setpoint (SET_ATTITUDE_TARGET) to
        the flight controller. The host forwards it to the MAVLink router, which
        encodes the SET_ATTITUDE_TARGET message. Gated on the
        ``flight.rate_setpoint`` capability."""
        return (
            await self._send_request(
                "flight.rate_setpoint.send",
                capability="flight.rate_setpoint",
                args=dict(setpoint),
            )
        ).args

    # ---- Config kv ----------------------------------------------------

    async def config_get(self, key: str, default: Any = None) -> Any:
        resp = await self._send_request(
            "config.get",
            capability="",
            args={"key": key, "default": default},
        )
        return resp.args.get("value")

    async def config_set(
        self, key: str, value: Any, scope: str = "drone"
    ) -> dict:
        return (
            await self._send_request(
                "config.set",
                capability="",
                args={"key": key, "value": value, "scope": scope},
            )
        ).args

    # ---- Process spawn (sandboxed) -----------------------------------

    async def process_spawn(
        self,
        basename: str,
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ) -> dict:
        """Authorize a vendor-binary spawn through the supervisor.

        The supervisor enforces the manifest allowlist and audit-logs
        the attempt. On success it returns the resolved install dir
        plus the original args/env; the actual exec happens in the
        plugin runner process via
        :func:`ados.plugins.process_sandbox.spawn` so the child
        inherits the runner's cgroup slice.
        """
        return (
            await self._send_request(
                "process.spawn",
                capability="process.spawn",
                args={
                    "basename": basename,
                    "args": list(args or []),
                    "env": dict(env or {}),
                },
            )
        ).args
