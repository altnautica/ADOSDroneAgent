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

Each capability ID also has a :class:`CapabilityMeta` entry in
:data:`CAPABILITY_CATALOG`. The catalog supplies human-readable
labels, descriptions, a coarse category, and a risk classification
that the install dialog renders. The REST endpoints (parse +
install) inline this metadata on each declared permission so the
GCS does not need to ship a parallel copy of the agent's catalog.

The catalog is authoritative: every entry in
:data:`AGENT_CAPABILITIES` must also exist in
:data:`CAPABILITY_CATALOG`. The module-level self-check at import
time guards against drift.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Optional, TypedDict

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
        # high-risk MAVLink component, estimator
        # injection, and explicit subprocess-spawn for vendor binaries.
        # Risk and full semantics live in the permission model spec.
        "mavlink.component.vio",
        "estimator.pose.inject",
        "process.spawn",
    }
)
"""Canonical agent permissions. Source: plugin permission model spec."""

ENFORCED_AGENT_CAPABILITIES: frozenset[str] = frozenset(
    {"event.publish", "event.subscribe"}
)
"""Subset that has runtime enforcement gates today. Other capabilities
are recorded and surfaced in the install dialog but no host-side gate
rejects calls based on them yet."""


class CapabilityMeta(TypedDict):
    """Human-readable metadata for one capability.

    ``label`` is a short action-verb sentence rendered as the
    permission row title in the install dialog. ``description`` is
    the body paragraph that explains what the permission unlocks and
    why an operator might care. ``category`` and ``risk`` drive
    grouping and the risk badge.
    """

    label: str
    description: str
    category: str  # "hardware" | "flight_control" | "data_network"
    #                 | "compute_process" | "ui_slot"
    risk: str  # "low" | "medium" | "high" | "critical"
    risk_reason: str


CAPABILITY_CATALOG: dict[str, CapabilityMeta] = {
    # ---- event bus ----------------------------------------------------
    "event.publish": {
        "label": "Publish events on the plugin event bus",
        "description": (
            "Lets the plugin emit events on topics it has been "
            "granted access to. Other plugins and host services that "
            "subscribe to the same topic will receive the payload."
        ),
        "category": "data_network",
        "risk": "low",
        "risk_reason": (
            "Bus messages are sandboxed and do not affect flight."
        ),
    },
    "event.subscribe": {
        "label": "Receive events from the plugin event bus",
        "description": (
            "Lets the plugin subscribe to topics it has been granted "
            "access to. The plugin sees payloads from other plugins "
            "and from host services that publish on those topics."
        ),
        "category": "data_network",
        "risk": "low",
        "risk_reason": "Read-only on a sandboxed in-process bus.",
    },
    # ---- mavlink ------------------------------------------------------
    "mavlink.read": {
        "label": "Read MAVLink messages from the flight controller",
        "description": (
            "Lets the plugin observe the live MAVLink message stream, "
            "including telemetry, mode changes, and status text. "
            "Useful for analytics, logging, and dashboards."
        ),
        "category": "flight_control",
        "risk": "low",
        "risk_reason": "Read-only on the MAVLink stream.",
    },
    "mavlink.write": {
        "label": "Send MAVLink commands to the flight controller",
        "description": (
            "Lets the plugin inject MAVLink commands to the autopilot, "
            "including mode changes, arming, and parameter writes. "
            "An untrusted plugin with this capability can take "
            "control of the aircraft."
        ),
        "category": "flight_control",
        "risk": "high",
        "risk_reason": (
            "Can change flight mode, arm motors, and alter "
            "parameters in flight."
        ),
    },
    "mavlink.component.camera": {
        "label": "Act as a MAVLink camera component",
        "description": (
            "Lets the plugin register itself as a MAVLink camera "
            "component and respond to camera-related MAVLink "
            "messages from the GCS and the autopilot."
        ),
        "category": "flight_control",
        "risk": "medium",
        "risk_reason": (
            "Owns a MAVLink component id; can interfere with the "
            "real camera if mis-configured."
        ),
    },
    "mavlink.component.gimbal": {
        "label": "Act as a MAVLink gimbal component",
        "description": (
            "Lets the plugin register itself as a MAVLink gimbal "
            "manager or device and handle gimbal control messages. "
            "Required by gimbal driver plugins."
        ),
        "category": "flight_control",
        "risk": "medium",
        "risk_reason": (
            "Owns a MAVLink component id and can move the gimbal."
        ),
    },
    "mavlink.component.payload": {
        "label": "Act as a MAVLink payload component",
        "description": (
            "Lets the plugin register itself as a MAVLink payload "
            "component for actuators, droppers, and other "
            "mission-specific hardware."
        ),
        "category": "flight_control",
        "risk": "medium",
        "risk_reason": (
            "Owns a MAVLink component id and can trigger "
            "payload actuators."
        ),
    },
    "mavlink.component.peripheral": {
        "label": "Act as a generic MAVLink peripheral component",
        "description": (
            "Lets the plugin claim a MAVLink component id for a "
            "peripheral that does not fit the camera, gimbal, or "
            "payload categories. Used by sensor bridges and "
            "experimental devices."
        ),
        "category": "flight_control",
        "risk": "medium",
        "risk_reason": (
            "Owns a MAVLink component id on the same bus as the "
            "autopilot."
        ),
    },
    "mavlink.component.vio": {
        "label": "Act as a MAVLink visual-inertial-odometry source",
        "description": (
            "Lets the plugin register as a VIO source that publishes "
            "pose estimates over MAVLink. The autopilot can fuse "
            "these directly into the EKF, so a faulty source can "
            "cause unsafe flight behavior."
        ),
        "category": "flight_control",
        "risk": "high",
        "risk_reason": (
            "Pose estimates feed the EKF and influence position "
            "control."
        ),
    },
    "estimator.pose.inject": {
        "label": "Inject pose estimates into the EKF",
        "description": (
            "Lets the plugin push pose samples (position, attitude, "
            "or both) directly into the autopilot state estimator. "
            "A bad pose stream produces unsafe position commands and "
            "fly-aways."
        ),
        "category": "flight_control",
        "risk": "high",
        "risk_reason": (
            "Directly biases the EKF; a malicious or buggy plugin "
            "can crash the aircraft."
        ),
    },
    # ---- telemetry ---------------------------------------------------
    "telemetry.read": {
        "label": "Read telemetry streams from the agent",
        "description": (
            "Lets the plugin read the same telemetry feed the GCS "
            "consumes (battery, GPS, attitude, mode). Useful for "
            "logging and analytics."
        ),
        "category": "data_network",
        "risk": "low",
        "risk_reason": "Read-only on existing telemetry topics.",
    },
    "telemetry.extend": {
        "label": "Publish new telemetry fields to the GCS",
        "description": (
            "Lets the plugin add new fields to the telemetry stream "
            "that ships to the GCS. Used by sensor plugins that "
            "surface custom readings on the dashboard."
        ),
        "category": "data_network",
        "risk": "low",
        "risk_reason": "Adds fields; does not alter existing ones.",
    },
    # ---- sensor registration -----------------------------------------
    "sensor.camera.register": {
        "label": "Register a camera as a system sensor",
        "description": (
            "Lets the plugin declare a camera device the rest of the "
            "agent and the GCS can address (live preview, recording, "
            "mission usage)."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Camera streams may be recorded and uploaded; sensor "
            "identity matters for safety."
        ),
    },
    "sensor.depth.register": {
        "label": "Register a depth sensor as a system sensor",
        "description": (
            "Lets the plugin declare a depth-camera or stereo "
            "sensor. Downstream services can consume depth maps for "
            "obstacle avoidance and SLAM."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Depth data may feed obstacle avoidance; faulty data "
            "can influence avoidance decisions."
        ),
    },
    "sensor.lidar.register": {
        "label": "Register a LiDAR as a system sensor",
        "description": (
            "Lets the plugin declare a LiDAR device. Downstream "
            "services can consume point clouds for mapping and "
            "obstacle avoidance."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Point clouds may feed obstacle avoidance; faulty data "
            "can influence avoidance decisions."
        ),
    },
    "sensor.imu.register": {
        "label": "Register an auxiliary IMU as a system sensor",
        "description": (
            "Lets the plugin declare a secondary IMU device the "
            "rest of the agent can address. Often paired with VIO "
            "and odometry workflows."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "IMU output may feed state estimation downstream."
        ),
    },
    "sensor.payload.register": {
        "label": "Register a payload device as a system sensor",
        "description": (
            "Lets the plugin declare a payload device (sprayer, "
            "dropper, sampler) so missions and the GCS can address it."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Payload identity drives mission actions; mis-registration "
            "can trigger the wrong actuator."
        ),
    },
    # ---- hardware ----------------------------------------------------
    "hardware.uart": {
        "label": "Open UART serial ports on the host",
        "description": (
            "Lets the plugin open and exchange data over UART serial "
            "ports. Required by GPS, range-finder, and modem drivers."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Serial ports may be shared with the autopilot or "
            "critical peripherals."
        ),
    },
    "hardware.i2c": {
        "label": "Read and write I2C devices on the host",
        "description": (
            "Lets the plugin talk to I2C devices on the SBC's I2C "
            "buses. Used by sensor drivers and small peripherals."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "I2C buses are shared; a misbehaving plugin can stall "
            "the bus for other devices."
        ),
    },
    "hardware.spi": {
        "label": "Read and write SPI devices on the host",
        "description": (
            "Lets the plugin talk to SPI devices on the SBC's SPI "
            "buses. Used by displays and high-rate sensor drivers."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "SPI buses are shared with on-board peripherals like the "
            "front-panel display."
        ),
    },
    "hardware.gpio": {
        "label": "Read and toggle GPIO pins on the host",
        "description": (
            "Lets the plugin read input pins and drive output pins "
            "on the SBC. Used by switches, LEDs, and discrete "
            "peripheral control."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "GPIO outputs can drive external hardware; mis-driven "
            "pins can damage attached devices."
        ),
    },
    "hardware.usb": {
        "label": "Claim raw USB devices on the host",
        "description": (
            "Lets the plugin attach to and exchange bulk transfers "
            "with USB devices. Required by vendor SDK drivers that "
            "talk to specific USB peripherals."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Claiming a USB device prevents other software from "
            "using it."
        ),
    },
    "hardware.usb.uvc": {
        "label": "Read frames from USB UVC cameras",
        "description": (
            "Lets the plugin capture video frames from USB Video "
            "Class cameras. Required by webcam and machine-vision "
            "drivers."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Video frames may be recorded, transmitted, or "
            "processed off-vehicle."
        ),
    },
    "hardware.camera.csi": {
        "label": "Read frames from MIPI CSI cameras",
        "description": (
            "Lets the plugin capture video frames from CSI-attached "
            "cameras on the SBC's camera connector. Required by "
            "native camera drivers."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Video frames may be recorded, transmitted, or "
            "processed off-vehicle."
        ),
    },
    "hardware.audio": {
        "label": "Access audio capture and playback devices",
        "description": (
            "Lets the plugin record from microphones or play to "
            "speakers connected to the host. Required by audio "
            "alert and voice plugins."
        ),
        "category": "hardware",
        "risk": "medium",
        "risk_reason": (
            "Audio capture may pick up private conversations near "
            "the aircraft."
        ),
    },
    # ---- vehicle -----------------------------------------------------
    "vehicle.command": {
        "label": "Issue high-level vehicle commands",
        "description": (
            "Lets the plugin send high-level vehicle commands "
            "(arm, takeoff, RTL, land, mode change) through the "
            "agent's command pipeline rather than raw MAVLink."
        ),
        "category": "flight_control",
        "risk": "high",
        "risk_reason": (
            "Can arm, take off, or land the aircraft."
        ),
    },
    # ---- mission -----------------------------------------------------
    "mission.read": {
        "label": "Read mission plans loaded on the agent",
        "description": (
            "Lets the plugin read the active mission, waypoints, "
            "fences, and rally points stored on the agent."
        ),
        "category": "flight_control",
        "risk": "low",
        "risk_reason": "Read-only on mission data.",
    },
    "mission.write": {
        "label": "Upload or modify mission plans on the agent",
        "description": (
            "Lets the plugin upload new missions, edit waypoints, "
            "or change fence and rally definitions. Used by "
            "mission-planner and pattern-generator plugins."
        ),
        "category": "flight_control",
        "risk": "high",
        "risk_reason": (
            "Mission changes drive autonomous flight paths."
        ),
    },
    # ---- network and filesystem --------------------------------------
    "network.outbound": {
        "label": "Open outbound network connections",
        "description": (
            "Lets the plugin make outbound TCP, UDP, and HTTP "
            "connections from the agent host. Required by plugins "
            "that talk to external services."
        ),
        "category": "data_network",
        "risk": "medium",
        "risk_reason": (
            "Exfiltration risk; outbound traffic can carry "
            "telemetry off the aircraft."
        ),
    },
    "filesystem.host": {
        "label": "Read and write files on the host filesystem",
        "description": (
            "Lets the plugin read and write files outside its own "
            "data directory. Required by plugins that ingest logs, "
            "maps, or other host-provided files."
        ),
        "category": "compute_process",
        "risk": "medium",
        "risk_reason": (
            "Broad host filesystem access; mis-use can corrupt "
            "agent state."
        ),
    },
    # ---- recording ---------------------------------------------------
    "recording.write": {
        "label": "Write recordings into the agent recording store",
        "description": (
            "Lets the plugin save video, telemetry, or analysis "
            "outputs into the agent's recording store so the GCS "
            "and post-flight tools can find them."
        ),
        "category": "data_network",
        "risk": "low",
        "risk_reason": (
            "Writes are sandboxed to the recording directory."
        ),
    },
    # ---- process spawn -----------------------------------------------
    "process.spawn": {
        "label": "Spawn subprocesses on the agent host",
        "description": (
            "Lets the plugin launch helper subprocesses listed in "
            "its manifest's spawn allowlist. Required to ship "
            "vendor-binary drivers and language runtimes outside "
            "the Python plugin host."
        ),
        "category": "compute_process",
        "risk": "high",
        "risk_reason": (
            "Subprocesses run outside the plugin host sandbox and "
            "with the plugin's privileges."
        ),
    },
}
"""Human-readable metadata for every entry in :data:`AGENT_CAPABILITIES`.

Keyed by capability id. See :class:`CapabilityMeta` for the field
contract. The install REST endpoints look entries up here and inline
them onto each declared permission in the response so the GCS does
not maintain a parallel mirror of the agent's catalog.
"""


# Self-check at import time: catalog completeness. The frozen
# capability set is the single source of truth, and every id in it
# must have a catalog entry. Drift produces a stack trace at import
# rather than silently shipping a permission with no human label.
_missing_catalog_entries = AGENT_CAPABILITIES - CAPABILITY_CATALOG.keys()
if _missing_catalog_entries:
    raise RuntimeError(
        "AGENT_CAPABILITIES missing CAPABILITY_CATALOG entries: "
        + ", ".join(sorted(_missing_catalog_entries))
    )
_orphan_catalog_entries = CAPABILITY_CATALOG.keys() - AGENT_CAPABILITIES
if _orphan_catalog_entries:
    raise RuntimeError(
        "CAPABILITY_CATALOG has entries not in AGENT_CAPABILITIES: "
        + ", ".join(sorted(_orphan_catalog_entries))
    )
del _missing_catalog_entries, _orphan_catalog_entries


def is_known_agent_capability(cap: str) -> bool:
    """Return True if the capability is declared in the catalog."""
    return cap in AGENT_CAPABILITIES


def get_capability_meta(cap_id: str) -> Optional[CapabilityMeta]:
    """Return the catalog entry for ``cap_id`` or ``None`` if unknown."""
    return CAPABILITY_CATALOG.get(cap_id)


def is_known_capability(cap_id: str) -> bool:
    """Return True if ``cap_id`` has a catalog entry.

    Equivalent to :func:`is_known_agent_capability` today since the
    catalog and the frozen capability set are kept in sync at import
    time, but exposed under a neutral name so REST callers that look
    up arbitrary ids do not have to import the agent-specific helper.
    """
    return cap_id in CAPABILITY_CATALOG


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
