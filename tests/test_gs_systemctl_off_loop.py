"""Ground-station pair transitions must not stall the API's loop.

They run inside ados-api, and each ``systemctl`` call blocks for as long as the
unit takes to stop or restart. Run inline, an unpair froze every other request
for the length of the whole restart.
"""

from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from ados.services.ground_station import pair_manager as pm


class _LoopWatch:
    """A 10 ms ticker plus a fake ``subprocess.run`` that records how far the
    ticker advanced while each call was blocking."""

    def __init__(self) -> None:
        self.ticks = 0
        self.advanced_per_call: list[int] = []

    def slow_run(self, cmd, **_kwargs):
        start = self.ticks
        time.sleep(0.1)
        self.advanced_per_call.append(self.ticks - start)
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    async def during(self, coro) -> None:
        done = asyncio.Event()

        async def _ticker() -> None:
            while not done.is_set():
                await asyncio.sleep(0.01)
                self.ticks += 1

        ticker = asyncio.create_task(_ticker())
        try:
            await coro
        finally:
            done.set()
            await ticker


@pytest.mark.asyncio
async def test_unpair_restarts_the_radio_off_the_loop(tmp_path, monkeypatch) -> None:
    watch = _LoopWatch()
    monkeypatch.setattr(pm.subprocess, "run", watch.slow_run)
    monkeypatch.setattr(pm, "_persist_pair_state", lambda **_kw: None)
    monkeypatch.setattr(pm, "_RELAY_SECRET_PATH", tmp_path / "relay.secret")

    await watch.during(pm.PairManager(key_dir=str(tmp_path)).unpair("gs"))

    assert watch.advanced_per_call
    assert min(watch.advanced_per_call) >= 3, watch.advanced_per_call
