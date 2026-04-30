"""Canonical capability catalog for ADOS plugins.

Authoritative list of named capabilities a plugin manifest may
declare. Manifest validation accepts strings here as opaque
permission identifiers; per-capability enforcement gates land
incrementally as the surfaces they protect ship.

Today ``event.publish`` and ``event.subscribe`` are enforced (see
:mod:`ados.plugins.events`). The rest are recorded in plugin state
and surfaced in the install dialog risk badge, but no runtime gate
rejects an action because of them yet. Treat the un-enforced caps
as advisory until the relevant subsystem ships its check.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados.plugins.errors import CapabilityDenied

if TYPE_CHECKING:
    from ados.plugins.supervisor import PluginSupervisor

AGENT_CAPABILITIES: frozenset[str] = frozenset(
    {
        # event bus (enforced today)
        "event.publish",
        "event.subscribe",
        # mavlink
        "mavlink.read",
        "mavlink.write",
        "mavlink.component.camera",
        "mavlink.component.gimbal",
        "mavlink.component.payload",
        "mavlink.component.peripheral",
        # telemetry
        "telemetry.read",
        "telemetry.extend",
        # sensor registration
        "sensor.camera.register",
        "sensor.depth.register",
        "sensor.lidar.register",
        "sensor.imu.register",
        "sensor.payload.register",
        # hardware
        "hardware.uart",
        "hardware.i2c",
        "hardware.spi",
        "hardware.gpio",
        "hardware.usb",
        "hardware.usb.uvc",
        "hardware.camera.csi",
        "hardware.audio",
        # vehicle
        "vehicle.command",
        # mission
        "mission.read",
        "mission.write",
        # network and filesystem
        "network.outbound",
        "filesystem.host",
        # recording
        "recording.write",
    }
)
"""Canonical agent permissions. Source: plugin permission model spec."""

ENFORCED_AGENT_CAPABILITIES: frozenset[str] = frozenset(
    {"event.publish", "event.subscribe"}
)
"""Subset that has runtime enforcement gates today. Other capabilities
are recorded and surfaced in the install dialog but no host-side gate
rejects calls based on them yet."""


def is_known_agent_capability(cap: str) -> bool:
    """Return True if the capability is declared in the catalog."""
    return cap in AGENT_CAPABILITIES


def get_granted_caps(
    supervisor: "PluginSupervisor", plugin_id: str
) -> set[str]:
    """Return the set of capability ids currently granted to plugin_id.

    Looks up the plugin's install record in supervisor state and
    extracts the granted permissions. Returns an empty set if the
    plugin is not installed.

    The supervisor IPC server already holds a verified
    :class:`CapabilityToken` per request and checks against
    ``token.granted_caps`` directly for the hot path. This helper is
    for non-IPC contexts (CLI, REST handlers, supervisor methods)
    that need the same view from state.
    """
    install = supervisor.find_install(plugin_id)
    if install is None:
        return set()
    return {pid for pid, grant in install.permissions.items() if grant.granted}


def has_capability(
    supervisor: "PluginSupervisor", plugin_id: str, cap: str
) -> bool:
    """Return True if ``cap`` is currently granted to ``plugin_id``."""
    return cap in get_granted_caps(supervisor, plugin_id)


def require_capability(
    supervisor: "PluginSupervisor", plugin_id: str, cap: str
) -> None:
    """Raise :class:`CapabilityDenied` if ``cap`` is not granted to ``plugin_id``."""
    if not has_capability(supervisor, plugin_id, cap):
        raise CapabilityDenied(plugin_id=plugin_id, capability=cap)
