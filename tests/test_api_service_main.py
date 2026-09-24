"""The ados-api entrypoint's lifecycle edges.

Two things the process must get right beyond serving routes: when the HTTP
server dies on its own the process has to exit (so systemd restarts it), and
the runtime-mode badge it persists has to follow the profile it seeds at boot.
"""

from __future__ import annotations

import asyncio
import sys
from types import SimpleNamespace

import pytest

import ados.services.api.__main__ as api_main


@pytest.mark.asyncio
async def test_seeding_the_profile_recomputes_the_runtime_badge(monkeypatch) -> None:
    """The badge is profile-scoped; the one computed before the seed is stale."""
    persisted: list[object] = []
    monkeypatch.setattr(api_main, "_PROFILE_SEED_DELAY_S", 0.0)
    monkeypatch.setattr(
        api_main, "_persist_runtime_mode", lambda config, log: persisted.append(config)
    )
    monkeypatch.setattr("ados.core.profile._read_profile_conf_value", lambda: None)
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.detect_profile",
        lambda _hint: {"profile": "ground_station", "source": "gpio"},
    )
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.write_profile_conf", lambda _result: True
    )
    config = SimpleNamespace(agent=SimpleNamespace(profile="auto"))
    log = SimpleNamespace(info=lambda *a, **k: None, warning=lambda *a, **k: None)

    await api_main._seed_profile_conf_if_unset(config, log)

    assert persisted == [config]


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
    monkeypatch.setattr(api_main, "_persist_runtime_mode", lambda *a: None)
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
        "ados.api.dual_bind",
        SimpleNamespace(make_listen_sockets=lambda *_a: []),
    )
    monkeypatch.setattr(api_main.uvicorn, "Config", lambda *a, **k: None)

    async def _no_seed(*_a) -> None:
        return None

    monkeypatch.setattr(api_main, "_seed_profile_conf_if_unset", _no_seed)

    code = await asyncio.wait_for(api_main.main(), timeout=2.0)

    assert code == 1
