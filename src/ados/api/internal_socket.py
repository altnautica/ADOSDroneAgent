"""The residual API's only listener: an internal Unix socket behind the front.

The native control front (``ados-control``) owns the LAN port, authenticates
every request, and reverse-proxies the few routes it does not serve itself to
this socket. The residual app carries no auth of its own, so it never opens a
TCP port: a peer that could reach it over the network would skip every gate the
front applies.

The path resolves the same way the front's proxy resolves it
(``crates/ados-control/src/proxy.rs::default_internal_socket``): an absolute
``ADOS_API_INTERNAL_SOCKET`` wins, otherwise ``<run dir>/api-internal.sock``.
"""

from __future__ import annotations

import grp
import os
import socket
from pathlib import Path

from ados.core.paths import ADOS_RUN_DIR

__all__ = [
    "API_INTERNAL_SOCKET_ENV",
    "OPERATOR_GROUP",
    "bind_internal_socket",
    "internal_socket_path",
]

#: An absolute override for the internal socket path. The installer's drop-in
#: sets it to the default; tests point it at a temp dir.
API_INTERNAL_SOCKET_ENV = "ADOS_API_INTERNAL_SOCKET"

_SOCKET_NAME = "api-internal.sock"

#: The group that owns the agent's command-plane sockets. The plugin users are
#: never members; the native front runs as root and reaches the socket anyway.
OPERATOR_GROUP = "ados-operator"


def internal_socket_path() -> Path:
    """The internal socket path: the env override, else under the run dir."""
    override = os.environ.get(API_INTERNAL_SOCKET_ENV, "").strip()
    if override:
        return Path(override)
    return ADOS_RUN_DIR / _SOCKET_NAME


def bind_internal_socket(path: Path | None = None, backlog: int = 2048) -> socket.socket:
    """Bind the stream ``AF_UNIX`` listener, mode 0o660, group ``ados-operator``.

    A stale socket file is removed first so a restart does not fail with
    ``EADDRINUSE``. The bind runs under a 0o177 umask so the socket is never
    reachable by anyone but the owner between ``bind`` and ``chmod``; the group
    grant follows once the group owns it. A dev host without the group still
    binds, owner-only.
    """
    target = path if path is not None else internal_socket_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    try:
        target.unlink()
    except FileNotFoundError:
        pass

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.setblocking(False)
    previous_umask = os.umask(0o177)
    try:
        sock.bind(str(target))
    finally:
        os.umask(previous_umask)
    sock.listen(backlog)

    try:
        gid = grp.getgrnam(OPERATOR_GROUP).gr_gid
        os.chown(target, -1, gid)
        os.chmod(target, 0o660)
    except (KeyError, OSError):
        pass
    return sock
