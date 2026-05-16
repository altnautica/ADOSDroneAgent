"""Main entry point for ADOS Drone Agent.

The implementation now lives in per-concern files alongside this
barrel:

* ``app.py`` — :class:`AgentApp` (lifecycle, task tracking, shutdown
  orchestration). Slim shell that delegates to free functions for
  the bulky paths.
* ``service_registry.py`` — :func:`register_services` (every service
  the agent spawns at startup, in order).
* ``heartbeat_payload.py`` — :func:`build_heartbeat_payload` (cloud
  heartbeat dict composer).
* ``cloud_loops.py`` — beacon / heartbeat / command-poll loops.
* ``signal_handlers.py`` — :func:`install_termination_handlers`
  (SIGTERM + SIGINT wiring).
* ``entry.py`` — :func:`main` (console-script entry point).
* ``_helpers.py`` — :func:`_get_local_ip`.

Existing callers (``from ados.core.main import AgentApp``,
``from ados.core.main import main``) keep working unchanged.
"""

from __future__ import annotations

from ._helpers import _get_local_ip
from .app import AgentApp
from .cloud_loops import (
    cloud_beacon_loop,
    cloud_command_poll_loop,
    cloud_heartbeat_loop,
)
from .entry import main
from .heartbeat_payload import build_heartbeat_payload
from .service_registry import register_services
from .signal_handlers import install_termination_handlers

__all__ = [
    "AgentApp",
    "main",
    "register_services",
    "build_heartbeat_payload",
    "cloud_beacon_loop",
    "cloud_heartbeat_loop",
    "cloud_command_poll_loop",
    "install_termination_handlers",
    "_get_local_ip",
]
