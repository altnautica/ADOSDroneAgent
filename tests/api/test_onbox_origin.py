"""The residual API refuses anything that did not arrive through the front.

The native control front owns the LAN port and authenticates every route it
serves or reverse-proxies; this app is the proxy target. That arrangement
rested on a property nothing checked, and two source comments asserted
opposite contracts about it. These tests assert the property directly, over a
real uvicorn bound to both an ``AF_UNIX`` and a TCP listener at once — a fake
ASGI scope would prove only that the middleware reads the dict it is handed.
"""

from __future__ import annotations

import asyncio
import os
import socket
import tempfile

import httpx
import pytest
import uvicorn
from fastapi import FastAPI

from ados.api.onbox_origin import OnboxOriginMiddleware


def _build_app() -> FastAPI:
    app = FastAPI()
    app.add_middleware(OnboxOriginMiddleware)

    @app.get("/probe")
    async def probe() -> dict[str, bool]:
        return {"ok": True}

    return app


class _DualListener:
    """A uvicorn serving one Unix socket and one TCP socket simultaneously.

    Both listeners feed the same app, which is the only way to compare the
    two transports without the comparison depending on how the app was
    constructed.
    """

    def __init__(self, app: FastAPI) -> None:
        self._app = app
        self._dir = tempfile.mkdtemp()
        self.uds_path = os.path.join(self._dir, "api-internal.sock")
        self.port = 0
        self._server: uvicorn.Server | None = None
        self._task: asyncio.Task[None] | None = None

    async def __aenter__(self) -> _DualListener:
        unix_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        unix_sock.bind(self.uds_path)
        unix_sock.listen(16)
        unix_sock.setblocking(False)

        tcp_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        tcp_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        tcp_sock.bind(("127.0.0.1", 0))
        self.port = tcp_sock.getsockname()[1]
        tcp_sock.listen(16)
        tcp_sock.setblocking(False)

        self._server = uvicorn.Server(
            uvicorn.Config(self._app, log_level="critical", access_log=False)
        )
        self._task = asyncio.create_task(
            self._server.serve(sockets=[unix_sock, tcp_sock])
        )
        for _ in range(100):
            if self._server.started:
                break
            await asyncio.sleep(0.02)
        return self

    async def __aexit__(self, *_exc: object) -> None:
        assert self._server is not None and self._task is not None
        self._server.should_exit = True
        await self._task

    async def over_unix(self, **kwargs: object) -> httpx.Response:
        transport = httpx.AsyncHTTPTransport(uds=self.uds_path)
        async with httpx.AsyncClient(
            transport=transport, base_url="http://internal"
        ) as client:
            return await client.get("/probe", **kwargs)  # type: ignore[arg-type]

    async def over_tcp(self, **kwargs: object) -> httpx.Response:
        async with httpx.AsyncClient(
            base_url=f"http://127.0.0.1:{self.port}"
        ) as client:
            return await client.get("/probe", **kwargs)  # type: ignore[arg-type]


@pytest.mark.asyncio
async def test_behind_the_front_only_the_internal_socket_is_served(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("ADOS_API_INTERNAL_SOCKET", "/run/ados/api-internal.sock")
    async with _DualListener(_build_app()) as srv:
        assert (await srv.over_unix()).status_code == 200
        # A direct TCP hit bypasses every gate the front applies, so it must
        # not reach a handler at all.
        assert (await srv.over_tcp()).status_code == 403


@pytest.mark.asyncio
async def test_a_client_cannot_claim_on_box_privilege_over_tcp(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Not behind the front: TCP is the intended way in for a dev run, and the
    # no-hardware path has to keep working. The spoofed marker is still
    # refused, because nothing legitimate sets it in this posture either.
    monkeypatch.delenv("ADOS_API_INTERNAL_SOCKET", raising=False)
    async with _DualListener(_build_app()) as srv:
        assert (await srv.over_tcp()).status_code == 200
        spoofed = await srv.over_tcp(headers={"X-ADOS-Onbox": "1"})
        assert spoofed.status_code == 403


@pytest.mark.asyncio
async def test_a_standalone_run_still_serves_tcp(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A dev run binds TCP with no front in front of it; refusing that would
    # break running the residual API on its own.
    monkeypatch.delenv("ADOS_API_INTERNAL_SOCKET", raising=False)
    async with _DualListener(_build_app()) as srv:
        assert (await srv.over_tcp()).status_code == 200
        assert (await srv.over_unix()).status_code == 200
