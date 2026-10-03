"""The shared cloudflared journal tail recovers after journalctl exits.

When the journalctl process ends, later subscribers must get a fresh one; a
tail that kept pointing at the dead process left the wizard's log console
blank for good.
"""

from __future__ import annotations

import asyncio
import sys

import pytest

from ados.api.routes.setup import cloud


@pytest.mark.asyncio
async def test_a_subscribe_after_journalctl_exits_spawns_a_new_one(monkeypatch) -> None:
    spawned: list[int] = []
    real_exec = asyncio.create_subprocess_exec

    async def short_lived_journal(*_argv, **kwargs):
        spawned.append(1)
        # Prints one line and exits, as a journalctl that died would.
        return await real_exec(sys.executable, "-c", "print('line')", **kwargs)

    monkeypatch.setattr(cloud.shutil, "which", lambda _name: "/usr/bin/journalctl")
    monkeypatch.setattr(cloud.asyncio, "create_subprocess_exec", short_lived_journal)

    tail = cloud._JournalTail("cloudflared")
    first = await tail.subscribe()
    assert await asyncio.wait_for(first.get(), timeout=5) == "line"
    assert await asyncio.wait_for(first.get(), timeout=5) == "(journal stream ended)"
    for _ in range(100):
        if tail._proc is None:
            break
        await asyncio.sleep(0.01)
    assert tail._proc is None

    second = await tail.subscribe()
    assert len(spawned) == 2
    assert await asyncio.wait_for(second.get(), timeout=5) == "line"
    await tail._terminate_proc()
