"""Plugin IPC submodules.

Splits the plugin IPC server's handlers out of the main server file
into per-surface modules so each handler family lives near the host
service it routes into. The server module keeps the connection /
handshake / dispatch loop; this package keeps the handler bodies.

Public surfaces:

* :mod:`ados.plugins.ipc.host_services` — facades the handlers route
  through (MAVLink router, component registrar, telemetry extender,
  driver registry, config kv store). The facades hide the concrete
  host modules and let tests inject fakes.
* :mod:`ados.plugins.ipc.handlers` — the handler functions themselves.
* :mod:`ados.plugins.ipc.process_handler` — the spawn handler is
  isolated because it carries the most threat surface.
"""

from __future__ import annotations

__all__: list[str] = []
