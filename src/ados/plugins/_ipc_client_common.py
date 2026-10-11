"""Wire constants, typed exceptions and helpers shared by the plugin IPC client."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from ados.core.logging import get_logger
from ados.plugins.errors import PluginError

if TYPE_CHECKING:
    import asyncio
    from collections.abc import Awaitable, Callable

    from ados.plugins.rpc import Envelope

# The wire method the host sends to ask the plugin to run one of its declared
# tools; the runner replies with a correlated response. Gated on the cap the
# generated table names for it, so the two sides cannot drift.
_TOOL_INVOKE_METHOD = "tool.invoke"

# The event method the host pushes a rotated capability token on. A token has a
# 600 s TTL and a plugin runs for a whole flight, so the host re-mints ahead of
# expiry and on every permission change; this is how the plugin's own copy
# stays current. The args are ``{token, expires_at, granted_caps}``.
#
# The plugin MUST adopt it: the token it presents on the next request is what
# the host gates against, and a stale one is refused. Before this existed
# nothing rotated at all, so every gated call from every plugin started
# failing ``token_expired`` ten minutes after the host began serving, with the
# plugin process still up and only per-call errors in its own log.
_TOKEN_REFRESH_METHOD = "token.refresh"

# The event topic the host pushes the vehicle-state snapshot on to a plugin
# that armed ``telemetry.subscribe`` (``ados_protocol::plugin::TELEMETRY_STATE_TOPIC``).
TELEMETRY_STATE_TOPIC = "telemetry.state"

# ---------------------------------------------------------------------
# Typed exceptions surfaced to plugin code
# ---------------------------------------------------------------------


class InvalidComponent(PluginError):
    """Raised when a plugin sends to a component_id it has not reserved."""


class AllowlistViolation(PluginError):
    """Raised when ``ctx.process.spawn`` rejects a basename that is not on
    the manifest's ``agent.subprocess_spawn`` allowlist."""

    def __init__(self, basename: str) -> None:
        super().__init__(
            f"binary basename {basename!r} not on subprocess_spawn allowlist"
        )
        self.basename = basename


log = get_logger("plugins.ipc_client")

DEFAULT_REQUEST_TIMEOUT_S = 5.0

#: Host-pushed deliveries waiting for the plugin's callbacks. Bounded so a
#: callback slower than the stream it subscribed to sheds the newest frames
#: instead of growing the runner without limit.
DELIVERY_QUEUE_MAX = 1024

#: Log one line per this many dropped deliveries, so a saturated callback is
#: visible without one log line per frame.
DELIVERY_DROP_LOG_EVERY = 100

#: The catalog capability each ``peripheral.register_driver`` kind needs.
_DRIVER_KIND_CAPS = {
    "camera": "sensor.camera.register",
    "lidar": "sensor.lidar.register",
    "gimbal": "sensor.payload.register",
    "gps": "sensor.payload.register",
    "esc": "sensor.payload.register",
    "payload-actuator": "sensor.payload.register",
}


def _driver_kind_capability(kind: str) -> str:
    """The catalog cap for a driver kind; ``ValueError`` for an unknown kind."""
    try:
        return _DRIVER_KIND_CAPS[kind]
    except KeyError:
        raise ValueError(f"unknown driver kind: {kind}") from None


class _IpcClientState:
    """Type-only view of the client state the method mixins share.

    :class:`ados.plugins.ipc_client.PluginIpcClient` owns and initialises every
    attribute; this only tells the type checker what the mixins may touch."""

    if TYPE_CHECKING:
        _plugin_id: str
        _token: str
        _writer: asyncio.StreamWriter | None
        _granted_caps: frozenset[str]
        _event_callbacks: dict[str, list[Callable[[dict[str, Any]], Awaitable[None] | None]]]
        _mavlink_callbacks: dict[str, list[Callable[[dict[str, Any]], Awaitable[None] | None]]]
        _detection_callbacks: list[Callable[[dict[str, Any]], Awaitable[None] | None]]
        _button_callbacks: list[Callable[[dict[str, Any]], Awaitable[None] | None]]
        _msp_callbacks: list[Callable[[dict[str, Any]], Awaitable[None] | None]]
        _tool_handlers: dict[str, Callable[[dict[str, Any]], Awaitable[Any] | Any]]

        async def _send_request(
            self,
            method: str,
            *,
            capability: str,
            args: dict[str, Any],
            timeout_s: float = ...,
        ) -> Envelope: ...
