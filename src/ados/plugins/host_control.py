"""Synchronous client for the plugin host daemon's on-box control socket.

Why this module exists
----------------------

The Python supervisor is the lifecycle controller: it installs, enables,
disables and grants. The ``ados-plugin-host`` daemon is what actually
*serves* a plugin — it binds ``/run/ados/plugins/<id>.sock`` and writes the
0600 token env file the plugin's systemd unit reads through
``EnvironmentFile=``.

State on disk is therefore not the enforcement point. A plugin's wire
capabilities live in a minted HMAC token the daemon holds, and its device /
socket / filesystem sandbox lives in a unit systemd has already exec'd. A
controller that only writes state has changed nothing a running plugin can
observe. That gap produced three failures, all of which this socket closes:

* ``enable`` started a plugin unit while nothing had bound its socket or
  written its token, so the runner found neither. The plugin looked active to
  systemd and ``running`` to the GCS and did nothing at all — no telemetry, no
  MAVLink, no config, and no error anywhere. :func:`reconcile` is called
  *before* the unit starts so both exist by the time the runner looks.
* Capability tokens have a 600 s TTL and nothing re-minted them, so every
  gated call from every plugin began failing ``token_expired`` ten minutes in.
  The daemon now rotates ahead of expiry on its own; this socket is how a
  permission change joins that cycle immediately.
* A revoke was advisory. Granted caps were baked into the token once per
  daemon start, so revoking ``mavlink.write`` from a misbehaving plugin
  reported success while the plugin kept commanding the flight controller.
  :func:`rotate_token` re-mints from the new grant set and pushes it into the
  live session, so the plugin's next request is gated against it.

Not reaching the daemon is not fatal
------------------------------------

The daemon re-reads plugin state on a fixed short poll regardless, so an
unreachable socket costs at most one poll interval of latency, never
correctness. Every function here returns a bool and never raises; callers log
the miss and carry on. The one thing a caller must NOT do is tell the operator
a change is applied when the socket was unreachable AND the poll has not run —
the supervisor surfaces the difference.

The wire is the same length-prefixed msgpack envelope every other agent IPC
socket speaks, so this needs no new framing and no event loop: a blocking
``socket`` with a short timeout is the whole client.
"""

from __future__ import annotations

import socket
from pathlib import Path
from typing import Any

import msgpack

from ados.core.logging import get_logger
from ados.core.paths import PLUGIN_RUN_DIR
from ados.plugins.rpc import MAX_FRAME_BYTES, Envelope, encode_frame

log = get_logger("plugins.host_control")

#: The control socket file name under the per-plugin socket dir. The leading
#: underscore keeps it out of the ``<plugin_id>.sock`` namespace.
CONTROL_SOCKET_NAME = "_control.sock"

#: Re-mint one plugin's capability token from the current grant set and push it
#: into the plugin's live session.
METHOD_TOKEN_ROTATE = "token.rotate"

#: Re-read plugin state and bring the daemon's served sockets in line with it.
METHOD_PLUGIN_RECONCILE = "plugin.reconcile"

#: One round trip. Generous against a loaded SBC, short enough that a wedged
#: daemon cannot stall an operator's CLI call.
TIMEOUT_S = 3.0


def control_socket_path(socket_dir: Path | None = None) -> Path:
    """The control socket path under ``socket_dir`` (default the run dir)."""
    base = socket_dir if socket_dir is not None else PLUGIN_RUN_DIR
    return Path(base) / CONTROL_SOCKET_NAME


def reconcile(socket_dir: Path | None = None) -> bool:
    """Ask the daemon to reconcile its served sockets against plugin state.

    Call this BEFORE ``systemctl start`` of a plugin unit: the socket and the
    token env file must exist before the runner looks for them.

    Returns True when the daemon acknowledged.
    """
    result = _request(METHOD_PLUGIN_RECONCILE, {}, socket_dir)
    if result is None:
        return False
    log.info(
        "plugin_host_reconciled",
        started=result.get("started"),
        stopped=result.get("stopped"),
        serving=result.get("serving"),
    )
    return True


def rotate_token(plugin_id: str, socket_dir: Path | None = None) -> bool:
    """Re-mint ``plugin_id``'s capability token from the current grant set.

    Returns True when the daemon acknowledged. The daemon also reports whether
    a live session received the token (``delivered``); a False there is the
    correct outcome for an enabled plugin that has not connected yet, so it is
    logged rather than treated as a failure.
    """
    result = _request(METHOD_TOKEN_ROTATE, {"plugin_id": plugin_id}, socket_dir)
    if result is None:
        return False
    log.info(
        "plugin_token_rotated",
        plugin_id=plugin_id,
        delivered=bool(result.get("delivered")),
    )
    return True


def _request(
    method: str, args: dict[str, Any], socket_dir: Path | None
) -> dict[str, Any] | None:
    """One request/response round trip. ``None`` on any failure.

    Never raises: the caller is a lifecycle path whose own work has already
    succeeded, and an unreachable plugin host is a latency problem the
    daemon's state poll resolves, not a reason to fail an install.
    """
    path = control_socket_path(socket_dir)
    env = Envelope(
        type="request",
        method=method,
        capability="",
        args=args,
        request_id=f"ctl-{method}",
        token="",
    )
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
            sock.settimeout(TIMEOUT_S)
            sock.connect(str(path))
            sock.sendall(encode_frame(env))
            header = _recv_exact(sock, 4)
            length = int.from_bytes(header, "big")
            if length == 0 or length > MAX_FRAME_BYTES:
                raise OSError(f"control response length {length} out of range")
            body = _recv_exact(sock, length)
    except (OSError, ValueError) as exc:
        log.info(
            "plugin_host_control_unreachable",
            method=method,
            socket=str(path),
            detail=str(exc),
        )
        return None

    raw = msgpack.unpackb(body, raw=False)
    if not isinstance(raw, dict):
        log.warning("plugin_host_control_bad_response", method=method)
        return None
    response = Envelope.from_dict(raw)
    if response.error:
        log.warning(
            "plugin_host_control_error", method=method, error=response.error
        )
        return None
    return response.args or {}


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise OSError(f"control socket closed after {len(buf)} of {n} bytes")
        buf += chunk
    return buf
