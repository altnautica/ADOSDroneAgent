"""Control-socket client for the WFB local-radio bind.

The bind rendezvous (stop the normal wfb unit, bring up the bind
profile, run the socat key exchange, flip back to the encrypted
profile) is driven by the supervisor process, which serves a Unix
control socket at ``/run/ados/supervisor.sock``. Every in-process
caller (REST handlers, the auto-pair supervisor, the LCD surface)
forwards its request to that socket instead of running the bind state
machine itself.

Wire protocol: one newline-terminated JSON request, one
newline-terminated JSON response per connection, then the server
closes.

    {"op": "start_bind", "role": "drone",
     "peer_device_id": null, "source": "operator"}
        -> blocks for the whole rendezvous ->
        {"ok": true, "session": {...}}
        (or {"ok": false, "error": "E_BIND_IN_PROGRESS"} if one is
        already running)

    {"op": "bind_status"}  -> {"ok": true, "session": {...}|null}

    {"op": "cancel_bind"}  -> {"ok": true}
        (sent on a SEPARATE connection to abort an in-flight
        start_bind that is blocked on its own connection)

A cross-process liveness sentinel at ``/run/ados/bind-state.json``
(a JSON object with a boolean ``"active"`` field) lets sync consumers
that cannot afford a socket round-trip (the hop supervisor's hot tick)
cheaply gate on "is a bind in flight right now".

The control socket is the sole producer of bind sessions. The
supervisor process serves it on every target, so an unreachable
socket is a hard error: ``forward_start_bind`` raises
``BindUnavailableError`` and ``forward_status`` returns ``{}`` (no
session info available). There is no in-process fallback.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("wfb.bind_client")

SUPERVISOR_SOCK = "/run/ados/supervisor.sock"
BIND_STATE_SENTINEL = "/run/ados/bind-state.json"

# Connecting to a live socket is local and near-instant; a short cap
# keeps a missing/refused socket from stalling the fallback decision.
_CONNECT_TIMEOUT_S = 2.0

# When the caller's cancel/timeout fires we send a cancel on a second
# connection and then re-await the original (blocked) read for the
# now-terminal session. Bound that re-await so a wedged server can't
# hang the caller forever.
_POST_CANCEL_READ_TIMEOUT_S = 10.0


class BindBusyError(RuntimeError):
    """Raised when a bind session is already in progress.

    Callers import the exception from this module so their
    ``except BindBusyError`` branches map onto the
    ``E_BIND_IN_PROGRESS`` reply the control socket returns.
    """


class BindUnavailableError(RuntimeError):
    """Raised when the bind control socket cannot be reached.

    The supervisor serves the socket on every target, so an
    unreachable socket means the producer is down. There is no
    in-process fallback; the caller surfaces this as a hard error.
    """


async def _open() -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
    """Connect to the control socket. Raises on absent/refused socket."""
    return await asyncio.wait_for(
        asyncio.open_unix_connection(SUPERVISOR_SOCK),
        timeout=_CONNECT_TIMEOUT_S,
    )


async def _send(writer: asyncio.StreamWriter, req: dict) -> None:
    """Write one newline-terminated JSON request and drain."""
    writer.write((json.dumps(req) + "\n").encode("utf-8"))
    await writer.drain()


async def _close(writer: asyncio.StreamWriter) -> None:
    """Close a writer, swallowing the usual teardown races."""
    writer.close()
    try:
        await writer.wait_closed()
    except Exception:  # noqa: BLE001 — teardown is best-effort
        pass


async def _send_cancel_best_effort() -> None:
    """Open a second connection and send cancel_bind, ignoring the reply.

    The blocked start_bind owns its own connection, so cancel must
    arrive on a separate one. Any failure here is non-fatal: the
    original read either returns the terminal session or we fall back
    to a status snapshot.
    """
    try:
        reader, writer = await _open()
    except (TimeoutError, OSError) as exc:
        log.debug("bind_cancel_connect_failed", error=str(exc))
        return
    try:
        await _send(writer, {"op": "cancel_bind"})
        try:
            await asyncio.wait_for(reader.readline(), timeout=_CONNECT_TIMEOUT_S)
        except (TimeoutError, OSError):
            pass
    finally:
        await _close(writer)


def _parse_start_reply(line: bytes) -> dict:
    """Parse a start_bind reply line into the session dict.

    Raises BindBusyError on E_BIND_IN_PROGRESS and RuntimeError on any
    other server-reported failure.
    """
    if not line:
        raise RuntimeError("control socket closed connection before replying")
    resp = json.loads(line.decode("utf-8"))
    if resp.get("ok") is False:
        error = resp.get("error") or "unknown bind error"
        if error == "E_BIND_IN_PROGRESS":
            raise BindBusyError("a bind session is already in progress")
        raise RuntimeError(error)
    return resp.get("session") or {}


async def forward_start_bind(
    *,
    role: str,
    source: str,
    peer_device_id: str | None,
    cancel_event: asyncio.Event | None,
    timeout: float | None,
) -> dict:
    """Forward a bind request to the control socket and block for the result.

    The call blocks for the whole rendezvous. The original read is raced
    against an optional ``cancel_event`` and an optional ``timeout``:
    whichever finishes first wins. If cancel or the timeout wins, a
    cancel is sent on a second connection and the original read is
    re-awaited for the now-terminal aborted/failed session, which is
    returned. The caller therefore always gets a session dict back on
    those paths rather than an exception.

    Raises ``BindUnavailableError`` when the control socket is
    unreachable: the socket is the sole producer of bind sessions.
    """
    try:
        reader, writer = await _open()
    except (TimeoutError, OSError) as exc:
        log.warning("bind_socket_unreachable", op="start_bind", error=str(exc))
        raise BindUnavailableError(
            "bind control socket unavailable at " + SUPERVISOR_SOCK
        ) from exc

    try:
        await _send(
            writer,
            {
                "op": "start_bind",
                "role": role,
                "peer_device_id": peer_device_id,
                "source": source,
            },
        )

        read_task = asyncio.ensure_future(reader.readline())
        waiters: list[asyncio.Future] = [read_task]
        cancel_task: asyncio.Task | None = None
        if cancel_event is not None:
            cancel_task = asyncio.ensure_future(cancel_event.wait())
            waiters.append(cancel_task)

        # The read_task is intentionally NOT cancelled on the cancel /
        # timeout paths below: we keep it alive to collect the
        # now-terminal session the server returns after we send cancel.
        done, _pending = await asyncio.wait(
            waiters,
            timeout=timeout,
            return_when=asyncio.FIRST_COMPLETED,
        )

        if read_task in done:
            if cancel_task is not None and not cancel_task.done():
                cancel_task.cancel()
            return _parse_start_reply(read_task.result())

        # Cancel fired, or the timeout elapsed (empty `done`). Abort the
        # in-flight session on a second connection, then re-await the
        # original read for the terminal session the server returns.
        if cancel_task is not None and not cancel_task.done():
            cancel_task.cancel()
        await _send_cancel_best_effort()
        try:
            line = await asyncio.wait_for(
                read_task, timeout=_POST_CANCEL_READ_TIMEOUT_S
            )
        except (TimeoutError, OSError):
            read_task.cancel()
            return await forward_status() or {}
        try:
            return _parse_start_reply(line)
        except BindBusyError:
            # A terminal reply that somehow still reads busy: treat as
            # no usable session and surface the latest snapshot.
            return await forward_status() or {}
    finally:
        await _close(writer)


async def forward_status() -> dict:
    """Return the latest bind-session snapshot, or ``{}`` if none.

    Status is best-effort: an unreachable control socket means "no
    session info available", which ``{}`` already represents.
    """
    try:
        reader, writer = await _open()
    except (TimeoutError, OSError) as exc:
        log.debug("bind_socket_unreachable", op="bind_status", error=str(exc))
        return {}

    try:
        await _send(writer, {"op": "bind_status"})
        try:
            line = await asyncio.wait_for(reader.readline(), timeout=_CONNECT_TIMEOUT_S)
        except (TimeoutError, OSError):
            return {}
        if not line:
            return {}
        resp = json.loads(line.decode("utf-8"))
        return resp.get("session") or {}
    finally:
        await _close(writer)


def read_bind_sentinel_active() -> bool:
    """Read the cross-process bind-liveness sentinel. Sync.

    Returns ``bool(obj["active"])`` from ``/run/ados/bind-state.json``,
    or ``False`` on any error (file missing, unreadable, or garbled).
    Cheap enough to call from a hot loop without a socket round-trip.
    """
    try:
        obj = json.loads(Path(BIND_STATE_SENTINEL).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    if not isinstance(obj, dict):
        return False
    return bool(obj.get("active"))
