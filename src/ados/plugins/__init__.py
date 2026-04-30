"""ADOS Drone Agent plugin host.

Plugins extend the agent without modifying core. A plugin is an
``.adosplug`` archive (signed zip) containing a ``manifest.yaml``,
optional Python wheel for the agent half, and optional GCS bundle.
The agent loads, validates, and supervises plugins via the
:class:`PluginSupervisor` in :mod:`ados.plugins.supervisor`.

Two install modes are supported:

* **Built-in plugins** are first-party Python packages registered via
  the ``ados.plugins`` entry-points group. They run in-process inside
  the supervisor when the manifest declares ``isolation: inprocess``.
* **Third-party plugins** are installed from ``.adosplug`` archives
  into ``/var/ados/plugins/<plugin-id>/``. Each one runs as a
  generated systemd unit (``ados-plugin-<id>.service``) inside a
  shared ``ados-plugins.slice`` cgroup slice. systemd handles
  restart-on-failure, resource limits (via slice and per-unit drops),
  and watchdog. The supervisor mediates lifecycle.

Public API surface kept narrow on purpose. Plugin authors consume
:mod:`ados_sdk` (the SDK package), not this module directly.
"""

from __future__ import annotations

from ados.plugins.errors import (
    ManifestError,
    PluginError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.manifest import (
    AgentBlock,
    GcsBlock,
    PluginManifest,
)

__all__ = [
    "AgentBlock",
    "GcsBlock",
    "ManifestError",
    "PluginError",
    "PluginManifest",
    "SignatureError",
    "SupervisorError",
]
