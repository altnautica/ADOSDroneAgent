"""ADOS Process Supervisor: manages child systemd services.

This package re-exports the public Supervisor surface from per-concern
modules. The barrel keeps `from ados.core.supervisor import Supervisor`
working for callers that already import it that way.

Modules:
  registry.py: ServiceSpec, SERVICE_REGISTRY, circuit-breaker constants
  hotplug.py:  HotplugMixin (USB add/remove routing)
  lifecycle.py: Supervisor class, run_supervisor, main entry point
"""

from .lifecycle import Supervisor, main, run_supervisor
from .registry import (
    FAILURE_WINDOW_SECS,
    MAX_FAILURES,
    SERVICE_REGISTRY,
    ServiceSpec,
)

__all__ = [
    "FAILURE_WINDOW_SECS",
    "MAX_FAILURES",
    "SERVICE_REGISTRY",
    "ServiceSpec",
    "Supervisor",
    "main",
    "run_supervisor",
]
