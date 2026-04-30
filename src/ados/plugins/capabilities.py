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
