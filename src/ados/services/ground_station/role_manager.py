"""Ground-station mesh role: the sentinel reader and the transition request.

A ground-station node runs one of three roles:

- `direct`: single-node RX (the `ados-wfb-rx` receive plane).
- `relay`: forwards WFB fragments to a receiver over batman-adv.
- `receiver`: aggregates fragments from the local NIC plus remote relays,
  FEC-combines them and feeds the mediamtx-gs pipeline.

The supervisor owns every role transition: it stops and starts the role's
systemd units, flips the `/etc/ados/mesh/role` sentinel and publishes the
`role_changed` mesh event. It is the one process no transition stops, so a
transition cannot kill the code running it. `apply_role` forwards the request
to the supervisor control socket and returns its result.
"""

from __future__ import annotations

import asyncio
import json

from ados.core.logging import get_logger
from ados.core.paths import MESH_ROLE_PATH
from ados.services.wfb.bind_client import SUPERVISOR_SOCK

log = get_logger("ground_station.role_manager")

ROLE_FILE = MESH_ROLE_PATH
VALID_ROLES: tuple[str, ...] = ("direct", "relay", "receiver")

# Connecting to a live local socket is near-instant.
_CONNECT_TIMEOUT_S = 2.0
# The transition stops and starts up to four units, each bounded by the
# supervisor's own systemctl ceiling, and waits behind an in-flight monitor pass.
_TRANSITION_TIMEOUT_S = 60.0


class RoleUnavailableError(RuntimeError):
    """The supervisor control socket did not answer, so no transition ran."""


def get_current_role() -> str:
    """Read the on-disk role sentinel.

    Falls back to `direct` if the sentinel is missing, unreadable, or
    contains an unknown value. This keeps boot-time behavior safe on
    a fresh install.
    """
    try:
        if ROLE_FILE.is_file():
            value = ROLE_FILE.read_text(encoding="utf-8").strip()
            if value in VALID_ROLES:
                return value
    except OSError:
        pass
    return "direct"


async def apply_role(target: str, *, reason: str = "operator") -> dict:
    """Ask the supervisor to move this node to `target` and wait for the result.

    Returns the transition metadata:
        {"role": "relay", "previous": "direct", "units_started": [...],
         "units_stopped": [...], "ts_ms": 1700000000000, "noop": False}

    Raises `ValueError` for an unknown role, `RoleUnavailableError` when the
    supervisor cannot be reached or does not answer, and `RuntimeError` when
    it refuses the transition (a bind owns the radio, not a ground station).
    """
    if target not in VALID_ROLES:
        raise ValueError(f"role must be one of {VALID_ROLES!r}, got {target!r}")

    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(SUPERVISOR_SOCK),
            timeout=_CONNECT_TIMEOUT_S,
        )
    except (TimeoutError, OSError) as exc:
        log.warning("role_socket_unreachable", error=str(exc))
        raise RoleUnavailableError(
            "supervisor control socket unavailable at " + SUPERVISOR_SOCK
        ) from exc

    try:
        request = {"op": "set_role", "role": target, "reason": reason}
        writer.write((json.dumps(request) + "\n").encode("utf-8"))
        await writer.drain()
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=_TRANSITION_TIMEOUT_S)
        except (TimeoutError, OSError) as exc:
            raise RoleUnavailableError("supervisor did not answer the role change") from exc
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except (OSError, ConnectionError):
            pass

    if not line:
        raise RoleUnavailableError("supervisor closed the connection without a reply")
    reply = json.loads(line.decode("utf-8"))
    if not reply.pop("ok", False):
        code = reply.get("error", "E_COMMAND_FAILED")
        if code == "E_INVALID_ROLE":
            raise ValueError(reply.get("message", code))
        raise RuntimeError(code)
    return reply
