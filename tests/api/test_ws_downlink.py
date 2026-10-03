"""A downlink-only WebSocket loop ends when the peer goes away, even if the
source it forwards from never produces another message."""

from __future__ import annotations

import asyncio

import pytest

from ados.api.ws_downlink import send_until_disconnect


class _PeerThatLeaves:
    """A WebSocket double whose peer disconnects once ``leave`` is set."""

    def __init__(self) -> None:
        self.leave = asyncio.Event()

    async def receive(self) -> dict:
        await self.leave.wait()
        return {"type": "websocket.disconnect", "code": 1001}


@pytest.mark.asyncio
async def test_a_disconnect_ends_a_send_loop_blocked_on_a_quiet_source() -> None:
    ws = _PeerThatLeaves()
    released = asyncio.Event()

    async def quiet_source() -> None:
        try:
            await asyncio.Event().wait()  # no message ever arrives
        finally:
            released.set()

    run = asyncio.ensure_future(send_until_disconnect(ws, quiet_source()))  # type: ignore[arg-type]
    await asyncio.sleep(0)
    ws.leave.set()
    await asyncio.wait_for(run, timeout=2.0)
    assert released.is_set()


@pytest.mark.asyncio
async def test_a_send_loop_error_reaches_the_caller() -> None:
    ws = _PeerThatLeaves()

    async def failing() -> None:
        raise ConnectionResetError("engine went away")

    with pytest.raises(ConnectionResetError):
        await send_until_disconnect(ws, failing())  # type: ignore[arg-type]
