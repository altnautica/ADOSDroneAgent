"""Helper for binding the REST API to BOTH IPv4 and IPv6 simultaneously.

uvicorn's `host` parameter creates a single AF_INET or AF_INET6 socket
depending on the literal it sees. On the bench Pi kernel, an `AF_INET6`
socket bound to `[::]` did not accept IPv4-mapped connections in
practice — `/proc/net/tcp` had no listener for the port and IPv4
clients (which is what most browsers try first when both A and AAAA
records exist for `*.local`) got a TCP RST. The result was a "Failed
to fetch" in the GCS even though IPv6 link-local connections worked.

This helper sidesteps the kernel/uvicorn dual-stack uncertainty by
opening two explicit sockets — one AF_INET, one AF_INET6 with
IPV6_V6ONLY set — and handing both to `uvicorn.Server.serve()` via
its `sockets=` argument. Either socket is independently sufficient
for clients that pick the matching family.
"""

from __future__ import annotations

import grp
import os
import socket

__all__ = [
    "make_dual_stack_sockets",
    "make_listen_sockets",
    "API_INTERNAL_SOCKET_ENV",
]

#: When this env var holds a path, the API binds that Unix socket instead of the
#: dual-stack TCP pair. The native HTTP front (ados-control) then owns the LAN
#: port and reverse-proxies the routes it has not yet taken over to this socket,
#: so a single CPython + FastAPI process serves behind the front while the
#: migration is in flight. Empty/unset → the default TCP listeners.
API_INTERNAL_SOCKET_ENV = "ADOS_API_INTERNAL_SOCKET"


def make_listen_sockets(
    ipv4_host: str,
    port: int,
    backlog: int = 2048,
) -> list[socket.socket]:
    """Resolve the API listener sockets for the current front posture.

    The default is the dual-stack TCP pair on ``port`` (see
    :func:`make_dual_stack_sockets`). When ``ADOS_API_INTERNAL_SOCKET`` is set,
    FastAPI serves behind the native front instead: this returns a single
    ``AF_UNIX`` listener bound at that path, so the residual Python API never
    touches the TCP port and the front reverse-proxies to it. uvicorn serves a
    pre-bound ``SOCK_STREAM`` socket of any family via its ``sockets=`` argument,
    so the call site stays unchanged.
    """
    internal = os.environ.get(API_INTERNAL_SOCKET_ENV, "").strip()
    if internal:
        return [_make_unix_socket(internal, backlog)]
    return make_dual_stack_sockets(ipv4_host, port, backlog)


def _make_unix_socket(path: str, backlog: int = 2048) -> socket.socket:
    """Bind a stream ``AF_UNIX`` listener at ``path``, 0o660 + the ``ados`` group.

    Removes a stale socket first so a restart does not fail with ``EADDRINUSE``,
    and group-owns to ``ados`` so the front (running as the ``ados`` user) can
    reach it while a stray local user cannot. The chmod/chown are best-effort: a
    dev host without the group still binds, just without the group grant.
    """
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.setblocking(False)
    sock.bind(path)
    sock.listen(backlog)

    # Group-own to ados first, then tighten the mode: the 0o660 grant only
    # reaches the front once the group owns the socket. Mirrors the native
    # control socket's bind in crates/ados-control/src/serve.rs::bind_unix.
    try:
        gid = grp.getgrnam("ados").gr_gid
        os.chown(path, -1, gid)
    except (KeyError, OSError):
        pass
    try:
        os.chmod(path, 0o660)
    except OSError:
        pass
    return sock


def make_dual_stack_sockets(
    ipv4_host: str,
    port: int,
    backlog: int = 2048,
) -> list[socket.socket]:
    """Create one AF_INET + one AF_INET6 listener for the same port.

    Returns the two sockets, ready to be passed to `uvicorn.Server.serve(
    sockets=...)`. The IPv6 socket explicitly sets IPV6_V6ONLY so the
    two sockets do not contend for the same connection table entry.

    If the IPv6 socket cannot be created (kernel built without IPv6, an
    unusual restriction in production environments), the function
    returns just the IPv4 socket so the agent still serves something.

    `ipv4_host` is the address family-specific bind address — typically
    `0.0.0.0` to listen on all IPv4 interfaces. Legacy configs may
    carry `::` here from an earlier release that tried single-socket
    dual-stack; normalize those to the IPv4 wildcard so the AF_INET
    bind doesn't fail with `gaierror: Address family ... not supported`.
    The IPv6 leg always binds `::` regardless.
    """
    # Normalize legacy "::" / IPv6 literals to the IPv4 wildcard so a
    # config that survived an earlier dual-stack release still boots.
    if ipv4_host in ("::", "[::]") or ":" in ipv4_host:
        ipv4_host = "0.0.0.0"

    sockets: list[socket.socket] = []

    v4 = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    v4.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    v4.setblocking(False)
    v4.bind((ipv4_host, port))
    v4.listen(backlog)
    sockets.append(v4)

    try:
        v6 = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
        v6.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        # Force the IPv6 socket to refuse IPv4-mapped connections so it
        # only handles real IPv6 traffic. The IPv4 socket above owns
        # IPv4. Without this, the second bind() may succeed on some
        # kernels but the two sockets contend ambiguously for IPv4
        # traffic.
        v6.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        v6.setblocking(False)
        v6.bind(("::", port))
        v6.listen(backlog)
        sockets.append(v6)
    except OSError:
        # No IPv6 support — fall back to IPv4-only. Caller still gets
        # a working listener; only IPv6 clients lose service.
        pass

    return sockets
