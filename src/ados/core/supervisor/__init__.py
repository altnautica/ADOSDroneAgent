"""Process orchestration is owned by the native ados-supervisor binary.

The long-running orchestrator (service lifecycle, USB hotplug routing,
health monitoring, heartbeat) runs as a standalone native binary started
by the ados-supervisor systemd unit. This package no longer ships a
Python orchestrator.
"""

__all__: list[str] = []
