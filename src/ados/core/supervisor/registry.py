"""Service registry data: ServiceSpec dataclass + the canonical service list.

Lifted out of the supervisor module so the catalog of managed services
can be inspected without importing the full Supervisor class.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field

# Circuit breaker thresholds. Stop restarting after MAX_FAILURES failures
# inside FAILURE_WINDOW_SECS.
MAX_FAILURES = 5
FAILURE_WINDOW_SECS = 60.0


@dataclass
class ServiceSpec:
    """Defines a managed service."""

    name: str
    category: str  # "core", "hardware", "suite", "ondemand"
    enabled: bool = True
    # profile_gate scopes the service to one agent profile.
    # None = runs on any profile. "drone" or "ground_station" gate it.
    profile_gate: str | None = None
    # role_gate scopes a ground-station service to one or more distributed
    # receive roles. None = runs on any role. Examples: "relay", "receiver",
    # or "relay|receiver" for units that cover both. Only consulted when
    # profile_gate == "ground_station".
    role_gate: str | None = None
    # Track failures for circuit breaker
    failure_times: deque[float] = field(default_factory=lambda: deque(maxlen=100))
    # Runtime state
    pid: int | None = None
    cpu_percent: float = 0.0
    memory_mb: float = 0.0
    uptime_seconds: float = 0.0
    state: str = "stopped"  # stopped, starting, running, failed, circuit_open


# All services the supervisor knows about
SERVICE_REGISTRY: list[dict] = [
    # Core (always running)
    {"name": "ados-mavlink", "category": "core"},
    {"name": "ados-api", "category": "core"},
    {"name": "ados-cloud", "category": "core"},
    {"name": "ados-health", "category": "core"},
    # Hardware-dependent (started on detection)
    {"name": "ados-video", "category": "hardware"},
    {"name": "ados-wfb", "category": "hardware"},
    # Suite-dependent (started on suite activation)
    {"name": "ados-scripting", "category": "suite"},
    # On-demand
    {"name": "ados-ota", "category": "ondemand"},
    {"name": "ados-discovery", "category": "ondemand"},
    # Peripheral Manager plugin registry. Cross-profile (no profile_gate);
    # peripherals exist on both drone and ground-station profiles.
    {"name": "ados-peripherals", "category": "hardware"},
    # Ground-station only services.
    # ados-wfb-rx is the single-node RX path. On relay/receiver nodes the
    # wfb_rx process is driven by ados-wfb-relay or ados-wfb-receiver
    # instead, so we gate this one to the direct role to keep both
    # processes from grabbing the same monitor-mode adapter.
    {"name": "ados-wfb-rx", "category": "hardware", "profile_gate": "ground_station", "role_gate": "direct"},
    {"name": "ados-mediamtx-gs", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-usb-gadget", "category": "hardware", "profile_gate": "ground_station"},
    # Physical UI + AP + first-boot captive portal.
    {"name": "ados-oled", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-buttons", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-hostapd", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-dnsmasq-gs", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-setup-captive", "category": "ondemand", "profile_gate": "ground_station"},
    # Standalone flight stack.
    {"name": "ados-kiosk", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-input", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-pic", "category": "hardware", "profile_gate": "ground_station"},
    # Uplink matrix and cloud relay. No `network` or `cloud` category exists
    # in the supervisor taxonomy (categories are core/hardware/suite/ondemand).
    # Uplink managers are hardware-like because they bind to real interfaces.
    # The cloud relay is treated as core because it always runs on the
    # ground-station profile, independent of hardware detection.
    {"name": "ados-uplink-router", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-modem", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-wifi-client", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-ethernet", "category": "hardware", "profile_gate": "ground_station"},
    {"name": "ados-cloud-relay", "category": "core", "profile_gate": "ground_station"},
    # Distributed receive role-gated services. ados-batman brings up
    # batman-adv for both relay and receiver. ados-wfb-relay forwards
    # fragments and is active on relay nodes only. ados-wfb-receiver
    # aggregates fragments and is active on receiver nodes only.
    {"name": "ados-batman", "category": "hardware", "profile_gate": "ground_station", "role_gate": "relay|receiver"},
    {"name": "ados-wfb-relay", "category": "hardware", "profile_gate": "ground_station", "role_gate": "relay"},
    {"name": "ados-wfb-receiver", "category": "hardware", "profile_gate": "ground_station", "role_gate": "receiver"},
    # ROS 2 environment (opt-in, Docker-managed).
    {"name": "ados-ros", "category": "suite"},
]
