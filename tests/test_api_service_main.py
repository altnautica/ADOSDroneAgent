"""The ados-api entrypoint's lifecycle edges.

Beyond serving routes, the process must get one thing right: when the HTTP
server dies on its own the process has to exit (so systemd restarts it).
"""

from __future__ import annotations

import asyncio
import sys
from types import SimpleNamespace

import pytest

import ados.services.api.__main__ as api_main


class _DeadServer:
    """A uvicorn server whose serve() returns at once (e.g. startup failed)."""

    def __init__(self, _config) -> None:
        self.should_exit = False

    async def serve(self, sockets=None) -> None:
        return None


class _IdleStateClient:
    connected = True

    async def connect(self, **_kwargs) -> None:
        return None

    async def read_loop(self) -> None:
        await asyncio.Event().wait()

    async def disconnect(self) -> None:
        return None


@pytest.mark.asyncio
async def test_the_process_exits_nonzero_when_the_http_server_dies(monkeypatch) -> None:
    """A process with no listener that stays up is never restarted by systemd."""
    config = SimpleNamespace(
        logging=SimpleNamespace(level="info"),
        agent=SimpleNamespace(profile="drone"),
        api=SimpleNamespace(rest=SimpleNamespace(host="127.0.0.1", port=0)),
    )
    monkeypatch.setattr(api_main, "load_config", lambda: config)
    monkeypatch.setattr(api_main, "configure_logging", lambda *a, **k: None)
    monkeypatch.setattr(api_main, "StateIPCClient", _IdleStateClient)
    monkeypatch.setattr(api_main.uvicorn, "Server", _DeadServer)
    # The route tree is irrelevant here; stand in for the three modules main()
    # imports so the test exercises only the process lifecycle.
    monkeypatch.setitem(
        sys.modules,
        "ados.api.runtime",
        SimpleNamespace(StandaloneApiRuntime=lambda *a: None),
    )
    monkeypatch.setitem(
        sys.modules, "ados.api.server", SimpleNamespace(create_app=lambda _r: None)
    )
    monkeypatch.setitem(
        sys.modules,
        "ados.api.internal_socket",
        SimpleNamespace(
            bind_internal_socket=lambda *_a: None,
            internal_socket_path=lambda: "/run/ados/api-internal.sock",
        ),
    )
    monkeypatch.setattr(api_main.uvicorn, "Config", lambda *a, **k: None)

    async def _no_seed(*_a) -> None:
        return None

    monkeypatch.setattr(api_main, "_seed_profile_conf_if_unset", _no_seed)

    code = await asyncio.wait_for(api_main.main(), timeout=2.0)

    assert code == 1
