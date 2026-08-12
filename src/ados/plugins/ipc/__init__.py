"""Plugin IPC submodules.

The host half of the plugin RPC surface is native: ``ados-plugin-host`` owns
the connection, handshake and dispatch loop, and binds the per-plugin sockets.
What remains here is the plugin-side half, which stays Python because the
plugin runtime does.

Public surfaces:

* :mod:`ados.plugins.ipc.context` — the ``ctx`` object a plugin is handed,
  wrapping the client's request/response calls in the namespaced API a plugin
  author writes against.
"""

from __future__ import annotations

__all__: list[str] = []
