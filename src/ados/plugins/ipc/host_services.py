"""Host-service facades for plugin IPC handlers.

The plugin IPC handlers do not talk to host services directly. They
talk to small facades defined here. Each facade is a Protocol-style
class with a thin default implementation that adapts the real host
module. Tests inject fakes by constructing :class:`HostServices` with
the desired stand-ins; the production wiring is :func:`default_host_services`.

Why this shape:

* The MAVLink router, peripheral registry, telemetry pump, and
  driver registries live in different parts of the agent. Putting
  one orchestration class between the IPC handler and the real
  modules keeps the IPC code testable without booting the full agent.
* Capability checks happen in the IPC dispatcher before the handler
  runs. The facade does not re-check; it is a thin pass-through.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

# ---------------------------------------------------------------------
# MAVLink
# ---------------------------------------------------------------------


# Message ids that the pose-injection path covers. The dispatcher
# checks this set and demands ``estimator.pose.inject`` in addition to
# ``mavlink.write`` for any send whose message id is in this set.
POSE_INJECT_MSG_IDS: frozenset[int] = frozenset(
    {
        331,    # ODOMETRY
        102,    # VISION_POSITION_ESTIMATE
        11011,  # VISION_POSITION_DELTA
        104,    # VICON_POSITION_ESTIMATE
        138,    # ATT_POS_MOCAP (vicon-equivalent attitude path)
    }
)


# Component ids that the VIO permission covers. Registering one of
# these requires ``mavlink.component.vio`` in addition to the
# matching component kind.
VIO_COMPONENT_IDS: frozenset[int] = frozenset({197, 198})


class MAVLinkRouter(Protocol):
    """Slice of the host's MAVLink connection the IPC handlers need.

    The production implementation wraps the native router's command socket
    (a ``send_bytes`` writer plus a subscribe queue). Tests provide an
    in-memory fake.
    """

    def send_bytes(self, data: bytes) -> None: ...

    def subscribe(self) -> Any: ...

    def unsubscribe(self, q: Any) -> None: ...


@dataclass
class ComponentRegistration:
    plugin_id: str
    component_id: int
    kind: str


class ComponentRegistrar:
    """Tracks per-plugin MAVLink component-id reservations.

    The dispatcher consults this when a plugin attempts to send with
    a non-default component_id; reservations must precede sends.
    """

    def __init__(self) -> None:
        self._by_plugin: dict[str, dict[int, ComponentRegistration]] = {}
        self._by_component_id: dict[int, ComponentRegistration] = {}

    def register(self, plugin_id: str, comp_id: int, kind: str) -> ComponentRegistration:
        existing = self._by_component_id.get(comp_id)
        if existing is not None and existing.plugin_id != plugin_id:
            raise PermissionError(
                f"component_id {comp_id} already reserved by {existing.plugin_id}"
            )
        reg = ComponentRegistration(plugin_id=plugin_id, component_id=comp_id, kind=kind)
        self._by_plugin.setdefault(plugin_id, {})[comp_id] = reg
        self._by_component_id[comp_id] = reg
        return reg

    def is_registered(self, plugin_id: str, comp_id: int) -> bool:
        return comp_id in self._by_plugin.get(plugin_id, {})

    def release_plugin(self, plugin_id: str) -> None:
        for comp_id in list(self._by_plugin.get(plugin_id, {})):
            self._by_component_id.pop(comp_id, None)
        self._by_plugin.pop(plugin_id, None)


# ---------------------------------------------------------------------
# Telemetry
# ---------------------------------------------------------------------


class TelemetryExtender:
    """Stores per-plugin telemetry channel payloads.

    The heartbeat builder reads ``snapshot()`` on each tick and merges
    the returned mapping into the ``extras.plugins`` heartbeat field.
    Plugins only ever add their own channel; channel keys are
    namespaced with the plugin id to make collisions impossible.
    """

    def __init__(self) -> None:
        self._channels: dict[str, dict[str, Any]] = {}

    def extend(self, plugin_id: str, channel: str, payload: dict[str, Any]) -> None:
        if not isinstance(channel, str) or not channel:
            raise ValueError("channel must be a non-empty string")
        key = f"{plugin_id}/{channel}"
        # Defensive copy so the plugin cannot mutate stored state
        # after the call returns.
        self._channels[key] = dict(payload)

    def clear_plugin(self, plugin_id: str) -> None:
        prefix = f"{plugin_id}/"
        for k in list(self._channels):
            if k.startswith(prefix):
                self._channels.pop(k, None)

    def snapshot(self) -> dict[str, dict[str, Any]]:
        return {k: dict(v) for k, v in self._channels.items()}


# ---------------------------------------------------------------------
# Driver registries
# ---------------------------------------------------------------------


DriverKind = str  # "camera" | "depth" | "lidar" | "imu" | "gimbal" | "gps" | "esc" | "payload"


@dataclass
class DriverHandle:
    plugin_id: str
    kind: DriverKind
    handle_id: str


class DriverRegistry:
    """Generic driver registry for camera / lidar / gimbal / gps / esc / payload-actuator.

    A single class covers every driver kind because all the host
    surface needs is install, lookup, and unregister-on-stop. The
    production agent has separate manager singletons (camera_mgr,
    peripheral registry, etc); the facade hands the driver instance
    to whichever manager owns the kind via the ``installer`` callable
    passed at construction time.
    """

    def __init__(
        self,
        installer: Callable[[DriverKind, str, Any], None] | None = None,
        uninstaller: Callable[[DriverKind, str, Any], None] | None = None,
    ) -> None:
        self._handles: dict[str, tuple[Any, DriverHandle]] = {}
        self._installer = installer
        self._uninstaller = uninstaller
        self._counter = 0

    def register(self, plugin_id: str, kind: DriverKind, driver: Any) -> DriverHandle:
        self._counter += 1
        handle_id = f"{kind}-{plugin_id}-{self._counter}"
        h = DriverHandle(plugin_id=plugin_id, kind=kind, handle_id=handle_id)
        self._handles[handle_id] = (driver, h)
        if self._installer is not None:
            self._installer(kind, plugin_id, driver)
        return h

    def unregister(self, handle_id: str) -> None:
        entry = self._handles.pop(handle_id, None)
        if entry is None:
            return
        driver, h = entry
        if self._uninstaller is not None:
            self._uninstaller(h.kind, h.plugin_id, driver)

    def release_plugin(self, plugin_id: str) -> None:
        for handle_id, (_, h) in list(self._handles.items()):
            if h.plugin_id == plugin_id:
                self.unregister(handle_id)


# ---------------------------------------------------------------------
# Camera claim
# ---------------------------------------------------------------------


@dataclass
class CameraClaim:
    plugin_id: str
    device_path: str
    exclusive: bool


@dataclass
class CameraFrame:
    """Latest captured frame for a claimed camera path.

    The capture source is owned by the video pipeline (libcamera or
    V4L2 — selected by board profile). This dataclass is the in-memory
    handoff shape between the capture loop and the IPC handler. ``data``
    is the raw image bytes in ``format``; the handler returns it verbatim
    to the plugin over msgpack.

    Real-world capture wiring lives outside of the plugin host. For now
    only the test harness populates this struct; on a production drone
    the video pipeline pushes a frame here on each new buffer it
    decodes, and the IPC ``camera.get_frame`` handler returns the
    cached value.
    """

    frame_id: int
    width: int
    height: int
    format: str
    data: bytes
    ts_ns: int


class CameraClaimTracker:
    """Records per-plugin exclusive holds on a /dev/videoN device.

    Domain F owns the camera_mgr extension that decides what the claim
    means at the encoder level. This tracker records who holds which
    path so a second plugin requesting exclusive on the same path is
    refused. It also acts as the cache point for the latest frame a
    plugin can poll via ``camera.get_frame``.
    """

    def __init__(self) -> None:
        self._claims: dict[str, CameraClaim] = {}
        self._frames: dict[str, CameraFrame] = {}

    def claim(self, plugin_id: str, device_path: str, exclusive: bool) -> CameraClaim:
        prior = self._claims.get(device_path)
        if prior is not None and prior.exclusive and prior.plugin_id != plugin_id:
            raise PermissionError(
                f"camera {device_path} is exclusively held by {prior.plugin_id}"
            )
        claim = CameraClaim(plugin_id=plugin_id, device_path=device_path, exclusive=exclusive)
        self._claims[device_path] = claim
        return claim

    def release(self, plugin_id: str, device_path: str) -> None:
        """Release a single path held by this plugin.

        No-op when the plugin does not hold the path; raises only when
        another plugin holds the path. Drops the cached frame so a
        stale buffer from a previous owner can't leak to the next
        claimant.
        """
        prior = self._claims.get(device_path)
        if prior is None:
            return
        if prior.plugin_id != plugin_id:
            raise PermissionError(
                f"camera {device_path} is held by {prior.plugin_id}, not {plugin_id}"
            )
        self._claims.pop(device_path, None)
        self._frames.pop(device_path, None)

    def release_plugin(self, plugin_id: str) -> None:
        for path, c in list(self._claims.items()):
            if c.plugin_id == plugin_id:
                self._claims.pop(path, None)
                self._frames.pop(path, None)

    def holder(self, device_path: str) -> str | None:
        """Return the plugin_id currently holding ``device_path``."""
        claim = self._claims.get(device_path)
        return claim.plugin_id if claim else None

    def publish_frame(self, device_path: str, frame: CameraFrame) -> None:
        """Stash the latest frame the capture loop produced for ``device_path``.

        The video pipeline calls this once per decoded buffer. Only
        called when a plugin holds the path — capture should not
        produce frames for an unclaimed device.
        """
        self._frames[device_path] = frame

    def latest_frame(self, device_path: str) -> CameraFrame | None:
        """Return the cached latest frame, or None if none has been published."""
        return self._frames.get(device_path)


# ---------------------------------------------------------------------
# Config (per-drone / global)
# ---------------------------------------------------------------------


@dataclass
class ConfigStore:
    """In-memory per-scope config store with optional persistence hook.

    Each plugin gets a ``per-plugin`` namespace. Within that namespace
    keys can be set at ``drone`` scope (one value per agent_id) or
    ``global`` scope (one value for the plugin regardless of which
    drone the plugin instance targets). Reads consult drone scope
    first, then global, then default.
    """

    persistence: Callable[[str, str, str, Any], None] | None = None
    _drone: dict[tuple[str, str, str], Any] = field(default_factory=dict)
    _global: dict[tuple[str, str], Any] = field(default_factory=dict)

    def get(
        self,
        plugin_id: str,
        key: str,
        *,
        agent_id: str = "",
        default: Any = None,
    ) -> Any:
        if agent_id:
            v = self._drone.get((plugin_id, agent_id, key), _MISSING)
            if v is not _MISSING:
                return v
        v = self._global.get((plugin_id, key), _MISSING)
        if v is not _MISSING:
            return v
        return default

    def set(
        self,
        plugin_id: str,
        key: str,
        value: Any,
        *,
        scope: str = "drone",
        agent_id: str = "",
    ) -> None:
        if scope == "drone":
            if not agent_id:
                # No drone bound; degrade gracefully to global.
                scope = "global"
        if scope == "drone":
            self._drone[(plugin_id, agent_id, key)] = value
        else:
            self._global[(plugin_id, key)] = value
        if self.persistence is not None:
            self.persistence(plugin_id, scope, key, value)

    def clear_plugin(self, plugin_id: str) -> None:
        for k in list(self._drone):
            if k[0] == plugin_id:
                self._drone.pop(k, None)
        for k in list(self._global):
            if k[0] == plugin_id:
                self._global.pop(k, None)


_MISSING = object()


# ---------------------------------------------------------------------
# Mavlink subscription pump
# ---------------------------------------------------------------------


class MAVLinkSubscriptionPump(Protocol):
    """Callable surface for streaming subscribed MAVLink messages back
    to the plugin runner. The IPC server provides a concrete
    implementation that wraps :class:`PluginSession.writer`."""

    async def push(self, plugin_id: str, msg_name: str, payload: dict[str, Any]) -> None: ...


# ---------------------------------------------------------------------
# Aggregate
# ---------------------------------------------------------------------


@dataclass
class HostServices:
    """Bundle of host-side service facades the IPC handlers route through.

    Constructed once by the supervisor at agent boot and passed into
    :class:`PluginIpcServer`. Tests construct a HostServices with
    stub facades.
    """

    mavlink: MAVLinkRouter | None = None
    components: ComponentRegistrar = field(default_factory=ComponentRegistrar)
    telemetry: TelemetryExtender = field(default_factory=TelemetryExtender)
    drivers: DriverRegistry = field(default_factory=DriverRegistry)
    cameras: CameraClaimTracker = field(default_factory=CameraClaimTracker)
    config: ConfigStore = field(default_factory=ConfigStore)
    # Lookup callable: given a plugin id, return the install directory
    # and the manifest subprocess_spawn allowlist. The IPC handler
    # uses these to enforce process.spawn at runtime.
    plugin_runtime_lookup: Callable[[str], tuple[Path, frozenset[str]]] | None = None
    # Per-plugin agent identity (cmd_drones._id). When the plugin is
    # not bound to a specific drone, returns the empty string.
    agent_id_lookup: Callable[[str], str] | None = None


def default_host_services() -> HostServices:
    """Build a HostServices populated with empty facades.

    The MAVLink router slot stays None and the runtime lookups stay
    None; the supervisor wires both in once the agent's main service
    surfaces have started. Until then handlers that hit a None slot
    return a structured ``not_available`` error.
    """
    return HostServices()


__all__ = [
    "POSE_INJECT_MSG_IDS",
    "VIO_COMPONENT_IDS",
    "MAVLinkRouter",
    "ComponentRegistrar",
    "ComponentRegistration",
    "TelemetryExtender",
    "DriverKind",
    "DriverHandle",
    "DriverRegistry",
    "CameraClaim",
    "CameraClaimTracker",
    "CameraFrame",
    "ConfigStore",
    "MAVLinkSubscriptionPump",
    "HostServices",
    "default_host_services",
]
