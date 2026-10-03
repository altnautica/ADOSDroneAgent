"""Run a downlink-only WebSocket send loop until either side ends.

A route that only sends (engine batches, log lines) never reads from the
socket, so a browser that closed its tab is noticed only on the next send. On
a quiet source that send may never come, and the route keeps its task and its
upstream connection forever. Racing the send loop against ``receive()`` ends
the loop the moment the peer disconnects.
"""

from __future__ import annotations

import asyncio
from collections.abc import Coroutine
from typing import Any

from fastapi import WebSocket


async def _until_peer_disconnects(websocket: WebSocket) -> None:
    while True:
        message = await websocket.receive()
        if message["type"] == "websocket.disconnect":
            return


async def send_until_disconnect(
    websocket: WebSocket, send_loop: Coroutine[Any, Any, None]
) -> None:
    """Await ``send_loop`` until it returns or the peer disconnects.

    Whichever finishes first cancels the other. An exception raised by the
    send loop propagates to the caller; the receive side's own errors (a
    receive after the socket already closed) only end the race.
    """
    sender = asyncio.ensure_future(send_loop)
    watcher = asyncio.ensure_future(_until_peer_disconnects(websocket))
    try:
        await asyncio.wait({sender, watcher}, return_when=asyncio.FIRST_COMPLETED)
    finally:
        for task in (sender, watcher):
            task.cancel()
        # `wait`, not `gather`: when this handler is itself being cancelled (the
        # server tearing the connection down), the CancelledError that leaves
        # here must be the caller's own. `gather` would replace it with one
        # rebuilt from a child's cancellation, which the server's cancel scope
        # no longer recognises as its own and lets escape.
        await asyncio.wait({sender, watcher})
    if not watcher.cancelled():
        watcher.exception()  # a receive-side error only ends the race
    error = None if sender.cancelled() else sender.exception()
    if error is not None:
        raise error
