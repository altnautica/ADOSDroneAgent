"""ADOS Drone Agent plugin support (Python side).

Plugins extend the agent without modifying core. A plugin is an
``.adosplug`` archive (signed zip) containing a ``manifest.yaml``,
an optional agent half, and an optional GCS bundle. The native plugin
host (``ados-plugin-host`` plus the ``/api/plugins`` lifecycle in
``ados-control``) installs, verifies, sandboxes and supervises them;
each subprocess plugin runs as a generated service unit.

What stays in Python: the manifest model (:mod:`ados.plugins.manifest`),
the archive packer/parser used by ``ados plugin sign`` and ``lint``, the
``ados-plugin-runner`` that hosts a Python agent half and its IPC client,
and the built-in plugins under :mod:`ados.plugins.builtin`.

Public API surface kept narrow on purpose. Plugin authors consume
:mod:`ados_sdk` (the SDK package), not this module directly.
"""

from __future__ import annotations

from ados.plugins.errors import (
    ManifestError,
    PluginError,
    SignatureError,
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
]
