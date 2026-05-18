"""Handler bodies for the plugin IPC server.

Each handler is a free async function taking ``(server, session,
env)``. The dispatch table lives in :mod:`ados.plugins.ipc_server`
and binds each method name to its handler plus the required capability.
Capability gating happens before the handler runs; handlers are
responsible only for argument validation, host-service routing, and
shaping the response dict.

Handlers raise :class:`_RpcError` (imported lazily to avoid an import
cycle) to signal a soft failure that becomes an envelope ``error``
field. Hard infrastructure failures (no MAVLink router yet, host
service missing) return a structured ``not_available`` dict so the
plugin can degrade gracefully.
"""

from __future__ import annotations

import asyncio
import struct
from typing import TYPE_CHECKING, Any

from ados.core.logging import get_logger
from ados.plugins.errors import CapabilityDenied
from ados.plugins.ipc.host_services import (
    POSE_INJECT_MSG_IDS,
    VIO_COMPONENT_IDS,
)
from ados.plugins.process_sandbox import (
    AllowlistViolation,
    SpawnError,
    spawn as sandbox_spawn,
)
from ados.plugins.rpc import Envelope, encode_frame

if TYPE_CHECKING:
    from ados.plugins.ipc_server import PluginIpcServer, PluginSession

log = get_logger("plugins.ipc.handlers")


# ---------------------------------------------------------------------
# MAVLink message id extraction
# ---------------------------------------------------------------------


def _mavlink_msg_id(frame: bytes) -> int | None:
    """Best-effort extraction of the MAVLink message id from a raw frame.

    Returns None if the frame is too short to classify; the caller
    routes None through normal ``mavlink.write`` gating without the
    extra pose-inject gate.

    MAVLink v2 frame layout:
        byte 0 = STX (0xFD)
        bytes 7..9 = msgid (little-endian 24-bit)

    MAVLink v1 frame layout:
        byte 0 = STX (0xFE)
        byte 5 = msgid (8-bit)

    The dispatcher does not validate signatures or CRCs; it only
    extracts msgid for permission classification.
    """
    if not frame:
        return None
    stx = frame[0]
    if stx == 0xFD and len(frame) >= 10:
        return int.from_bytes(frame[7:10], "little")
    if stx == 0xFE and len(frame) >= 6:
        return int(frame[5])
    return None


# ---------------------------------------------------------------------
# MAVLink handlers
# ---------------------------------------------------------------------


async def handle_mavlink_send(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    msg_bytes = env.args.get("msg_bytes")
    component_id = env.args.get("component_id")
    if isinstance(msg_bytes, list):
        # msgpack may decode bytes-of-ints as a list on some configs.
        try:
            msg_bytes = bytes(msg_bytes)
        except (TypeError, ValueError) as exc:
            raise _rpc_error(f"msg_bytes coercion failed: {exc}")
    if not isinstance(msg_bytes, (bytes, bytearray)):
        raise _rpc_error("msg_bytes must be bytes")
    msg_bytes = bytes(msg_bytes)
    if not msg_bytes:
        raise _rpc_error("msg_bytes must be non-empty")
    # Pose-inject gate: rejects ungranted callers regardless of mavlink.write.
    msg_id = _mavlink_msg_id(msg_bytes)
    if msg_id in POSE_INJECT_MSG_IDS:
        if "estimator.pose.inject" not in session.token.granted_caps:
            raise CapabilityDenied(session.plugin_id, "estimator.pose.inject")
    # Component-id reservation check.
    if component_id is not None:
        try:
            comp_id_int = int(component_id)
        except (TypeError, ValueError) as exc:
            raise _rpc_error(f"component_id not integer: {exc}")
        if comp_id_int in VIO_COMPONENT_IDS:
            if "mavlink.component.vio" not in session.token.granted_caps:
                raise CapabilityDenied(session.plugin_id, "mavlink.component.vio")
        if not server.host.components.is_registered(session.plugin_id, comp_id_int):
            raise _rpc_error(
                f"component_id {comp_id_int} not reserved by {session.plugin_id}; "
                "call mavlink.register_component first"
            )
    router = server.host.mavlink
    if router is None:
        return {"error": "not_available", "method": "mavlink.send"}
    try:
        router.send_bytes(msg_bytes)
    except Exception as exc:  # noqa: BLE001
        log.warning(
            "plugin_mavlink_send_failed",
            plugin_id=session.plugin_id,
            error=str(exc),
        )
        return {"error": "send_failed", "detail": str(exc)}
    return {"sent": True, "len": len(msg_bytes)}


async def handle_mavlink_subscribe(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    msg_name = env.args.get("msg_name")
    if not isinstance(msg_name, str) or not msg_name:
        raise _rpc_error("msg_name must be a non-empty string")
    if msg_name in session.mavlink_subscriptions:
        return {"already_subscribed": True}
    session.mavlink_subscriptions.add(msg_name)
    server.spawn_mavlink_pump(session, msg_name)
    return {"subscribed": True, "msg_name": msg_name}


async def handle_mavlink_register_component(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    comp_id = env.args.get("component_id")
    kind = env.args.get("kind")
    if not isinstance(kind, str) or not kind:
        raise _rpc_error("kind must be a non-empty string")
    try:
        comp_id_int = int(comp_id)
    except (TypeError, ValueError) as exc:
        raise _rpc_error(f"component_id not integer: {exc}")
    required = f"mavlink.component.{kind}"
    if required not in session.token.granted_caps:
        raise CapabilityDenied(session.plugin_id, required)
    if comp_id_int in VIO_COMPONENT_IDS and kind != "vio":
        raise _rpc_error(
            f"component_id {comp_id_int} is reserved for kind=vio"
        )
    try:
        reg = server.host.components.register(
            session.plugin_id, comp_id_int, kind
        )
    except PermissionError as exc:
        raise _rpc_error(str(exc))
    return {
        "registered": True,
        "component_id": reg.component_id,
        "kind": reg.kind,
    }


# ---------------------------------------------------------------------
# Telemetry handlers
# ---------------------------------------------------------------------


async def handle_telemetry_extend(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    channel = env.args.get("channel")
    payload = env.args.get("payload") or {}
    if not isinstance(channel, str) or not channel:
        raise _rpc_error("channel must be a non-empty string")
    if not isinstance(payload, dict):
        raise _rpc_error("payload must be a mapping")
    try:
        server.host.telemetry.extend(
            session.plugin_id, channel, payload
        )
    except ValueError as exc:
        raise _rpc_error(str(exc))
    return {"merged": True, "channel": channel}


# ---------------------------------------------------------------------
# Peripheral / driver registration handlers
# ---------------------------------------------------------------------


_DRIVER_KIND_TO_CAP: dict[str, str] = {
    "camera": "sensor.camera.register",
    "depth": "sensor.depth.register",
    "lidar": "sensor.lidar.register",
    "imu": "sensor.imu.register",
    "payload": "sensor.payload.register",
    # gimbal / gps / esc / payload-actuator do not have dedicated
    # sensor.* permissions in the v1.0 catalog; they reuse the
    # appropriate sensor.* register cap via the manifest. Until
    # 04-permission-model formalizes these we accept any of the
    # existing sensor.* caps as authorization.
    "gimbal": "sensor.payload.register",
    "gps": "sensor.payload.register",
    "esc": "sensor.payload.register",
    "payload-actuator": "sensor.payload.register",
}


async def handle_peripheral_register_driver(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    """Register a driver of any kind.

    The IPC payload carries an opaque ``driver_ref`` (a short string
    the plugin runner side resolves to the local driver instance) and
    the kind. The host facade stores the registration and the
    driver-side hot path (frame pump, MAVLink command emit, etc.)
    keeps running in the plugin process; this RPC simply makes the
    driver discoverable to the host's manager.
    """
    kind = env.args.get("kind")
    driver_ref = env.args.get("driver_ref")
    if not isinstance(kind, str) or not kind:
        raise _rpc_error("kind must be a non-empty string")
    if not isinstance(driver_ref, str) or not driver_ref:
        raise _rpc_error("driver_ref must be a non-empty string")
    required = _DRIVER_KIND_TO_CAP.get(kind)
    if required is None:
        raise _rpc_error(f"unknown driver kind: {kind}")
    if required not in session.token.granted_caps:
        raise CapabilityDenied(session.plugin_id, required)
    handle = server.host.drivers.register(
        plugin_id=session.plugin_id, kind=kind, driver=driver_ref
    )
    return {
        "registered": True,
        "kind": kind,
        "handle_id": handle.handle_id,
    }


async def handle_peripheral_unregister_driver(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    handle_id = env.args.get("handle_id")
    if not isinstance(handle_id, str) or not handle_id:
        raise _rpc_error("handle_id must be a non-empty string")
    server.host.drivers.unregister(handle_id)
    return {"unregistered": True}


# ---------------------------------------------------------------------
# Camera claim handler
# ---------------------------------------------------------------------


async def handle_camera_claim(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    device_path = env.args.get("device_path")
    exclusive = bool(env.args.get("exclusive", True))
    if not isinstance(device_path, str) or not device_path:
        raise _rpc_error("device_path must be a non-empty string")
    try:
        claim = server.host.cameras.claim(
            session.plugin_id, device_path, exclusive
        )
    except PermissionError as exc:
        raise _rpc_error(str(exc))
    return {
        "claimed": True,
        "device_path": claim.device_path,
        "exclusive": claim.exclusive,
    }


async def handle_camera_release(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    device_path = env.args.get("device_path")
    if not isinstance(device_path, str) or not device_path:
        raise _rpc_error("device_path must be a non-empty string")
    try:
        server.host.cameras.release(session.plugin_id, device_path)
    except PermissionError as exc:
        raise _rpc_error(str(exc))
    return {"released": True, "device_path": device_path}


# Supported frame formats. The plugin requests one of these in
# get_frame; the supervisor refuses anything else so a future format
# bump is a versioned addition rather than an opaque pass-through.
_SUPPORTED_CAMERA_FORMATS: set[str] = {"nv12", "rgb888", "yuv420p"}


async def handle_camera_get_frame(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    device_path = env.args.get("device_path")
    fmt = env.args.get("format", "nv12")
    timeout_ms_raw = env.args.get("timeout_ms", 1000)
    if not isinstance(device_path, str) or not device_path:
        raise _rpc_error("device_path must be a non-empty string")
    if not isinstance(fmt, str) or fmt not in _SUPPORTED_CAMERA_FORMATS:
        raise _rpc_error(
            f"format {fmt!r} not supported; pick one of "
            f"{sorted(_SUPPORTED_CAMERA_FORMATS)}"
        )
    try:
        timeout_ms = int(timeout_ms_raw)
    except (TypeError, ValueError):
        raise _rpc_error("timeout_ms must be an integer") from None
    if timeout_ms < 0:
        raise _rpc_error("timeout_ms must be >= 0")

    holder = server.host.cameras.holder(device_path)
    if holder is None:
        raise _rpc_error(
            f"camera {device_path} is not claimed; call camera.claim first"
        )
    if holder != session.plugin_id:
        raise _rpc_error(
            f"camera {device_path} is held by another plugin ({holder})"
        )

    frame = server.host.cameras.latest_frame(device_path)
    if frame is None:
        raise _rpc_error(
            f"no frame available for {device_path}; capture pipeline has not "
            "produced a buffer yet"
        )
    if frame.format != fmt:
        raise _rpc_error(
            f"frame format mismatch: pipeline produced {frame.format!r}, "
            f"plugin requested {fmt!r}"
        )
    # `stale` reflects whether the supervisor would have liked to wait
    # for a new frame but is returning the cached one. The capture
    # pipeline owns the "is this fresh" computation; today we hand back
    # whatever is cached so plugins can implement their own age check
    # against `ts_ns`. timeout_ms is accepted for forward-compat with
    # the eventual blocking-wait variant.
    _ = timeout_ms
    return {
        "frame_id": frame.frame_id,
        "width": frame.width,
        "height": frame.height,
        "format": frame.format,
        "data": frame.data,
        "ts_ns": frame.ts_ns,
        "stale": False,
    }


# ---------------------------------------------------------------------
# Config kv handler
# ---------------------------------------------------------------------


async def handle_config_get(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    key = env.args.get("key")
    default = env.args.get("default")
    if not isinstance(key, str) or not key:
        raise _rpc_error("key must be a non-empty string")
    agent_id = _agent_id_for(server, session.plugin_id)
    value = server.host.config.get(
        session.plugin_id, key, agent_id=agent_id, default=default
    )
    return {"value": value}


async def handle_config_set(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    key = env.args.get("key")
    if not isinstance(key, str) or not key:
        raise _rpc_error("key must be a non-empty string")
    if "value" not in env.args:
        raise _rpc_error("value missing")
    scope = env.args.get("scope") or "drone"
    if scope not in ("drone", "global"):
        raise _rpc_error(f"scope must be drone or global, got {scope!r}")
    agent_id = _agent_id_for(server, session.plugin_id)
    server.host.config.set(
        session.plugin_id,
        key,
        env.args["value"],
        scope=scope,
        agent_id=agent_id,
    )
    return {"set": True, "scope": scope}


def _agent_id_for(server: "PluginIpcServer", plugin_id: str) -> str:
    lookup = server.host.agent_id_lookup
    if lookup is None:
        return ""
    try:
        return lookup(plugin_id) or ""
    except Exception:  # noqa: BLE001
        return ""


# ---------------------------------------------------------------------
# Process spawn handler
# ---------------------------------------------------------------------


async def handle_process_spawn(
    server: "PluginIpcServer", session: "PluginSession", env: Envelope
) -> dict[str, Any]:
    """Spawn a vendor binary on behalf of the plugin runner.

    Plugin-side ergonomics: the runner already lives in the plugin's
    cgroup slice; spawning the child here would put the child in the
    *supervisor's* cgroup, which is wrong. So the supervisor returns
    a structured ``proxy_to_runner`` response telling the runner to
    perform the spawn itself in its own address space. The allowlist
    enforcement, audit log, and capability gating still happen here.

    The supervisor records the spawn attempt so audit logs survive
    even if the runner-side spawn fails. The runner then invokes
    :func:`ados.plugins.process_sandbox.spawn` locally.
    """
    basename = env.args.get("basename")
    args = env.args.get("args") or []
    spawn_env = env.args.get("env") or {}
    if not isinstance(basename, str) or not basename:
        raise _rpc_error("basename must be a non-empty string")
    if not isinstance(args, list):
        raise _rpc_error("args must be a list of strings")
    if not isinstance(spawn_env, dict):
        raise _rpc_error("env must be a mapping")

    lookup = server.host.plugin_runtime_lookup
    if lookup is None:
        return {"error": "not_available", "method": "process.spawn"}
    try:
        install_dir, allowlist = lookup(session.plugin_id)
    except KeyError:
        return {
            "error": "not_available",
            "method": "process.spawn",
            "reason": "plugin runtime not registered",
        }

    if basename not in allowlist:
        log.warning(
            "plugin_process_spawn_denied",
            plugin_id=session.plugin_id,
            basename=basename,
            allowlist_size=len(allowlist),
        )
        raise AllowlistViolation(
            plugin_id=session.plugin_id, basename=basename
        )

    # Audit log the approved spawn intent before the runner exec.
    log.info(
        "plugin_process_spawn_authorized",
        plugin_id=session.plugin_id,
        basename=basename,
    )

    return {
        "authorized": True,
        "install_dir": str(install_dir),
        "basename": basename,
        "args": list(args),
        "env": dict(spawn_env),
    }


# ---------------------------------------------------------------------
# RpcError thin import (kept here to avoid circular import at module load)
# ---------------------------------------------------------------------


def _rpc_error(message: str) -> Exception:
    """Lazily import _RpcError from ipc_server to avoid a cycle.

    ipc_server imports this module to populate its dispatch table, so
    importing _RpcError at module top would loop. We import at call
    time, which is a no-op once the module is loaded.
    """
    from ados.plugins.ipc_server import _RpcError

    return _RpcError(message)


__all__ = [
    "handle_mavlink_send",
    "handle_mavlink_subscribe",
    "handle_mavlink_register_component",
    "handle_telemetry_extend",
    "handle_peripheral_register_driver",
    "handle_peripheral_unregister_driver",
    "handle_camera_claim",
    "handle_camera_release",
    "handle_camera_get_frame",
    "handle_config_get",
    "handle_config_set",
    "handle_process_spawn",
]


# Suppress unused-import warnings for utilities consumed dynamically.
_ = asyncio
_ = struct
_ = encode_frame
_ = SpawnError
_ = sandbox_spawn
