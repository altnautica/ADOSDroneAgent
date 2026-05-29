"""Tests for the auto_pair supervisor's local-bind failover state.

The supervisor lives in the ados-cloud process and writes a sidecar
file to /run/ados/wfb_failover.json so the ados-api process can serve
the current state over REST. These tests skip the 15-second settle
delay, point the sidecar at a tmp_path, and stub the heavy collaborators
(load_config, pair_manager, adapter detection, the bind-client forward).
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any

import pytest

from ados.services.wfb import auto_pair as auto_pair_mod
from ados.services.wfb.auto_pair import (
    MAX_LOCAL_BIND_ATTEMPTS,
    AutoPairSupervisor,
)


class _StubPairManager:
    """Minimal pair_manager double that always reports unpaired."""

    def __init__(self) -> None:
        self.set_auto_pair_calls: list[tuple[bool, str]] = []

    async def status(self, role: str) -> dict[str, Any]:
        return {"paired": False, "role": role}

    async def set_auto_pair(self, enabled: bool, role: str) -> dict[str, Any]:
        self.set_auto_pair_calls.append((enabled, role))
        return {"auto_pair_enabled": enabled}


class _StubForwardBind:
    """Stand-in for ``bind_client.forward_start_bind`` with a script.

    Each invocation pops the next entry from the script. Entries can be
    either a result dict (terminal state) or a callable that returns one
    (used to raise on a specific attempt). The instance is callable so it
    drops directly into the supervisor's ``forward_start_bind`` lookup.
    """

    def __init__(self, script: list[Any]) -> None:
        self._script = list(script)
        self.call_count = 0

    async def __call__(self, **_: Any) -> dict[str, Any]:
        self.call_count += 1
        if not self._script:
            return {"state": "failed", "error": "exhausted"}
        next_entry = self._script.pop(0)
        if callable(next_entry):
            return next_entry()
        return next_entry


class _FakeAdapter:
    is_wfb_compatible = True


class _StubWfbCfg:
    auto_pair_enabled = True


class _StubVideo:
    wfb = _StubWfbCfg()


class _StubConfig:
    video = _StubVideo()


def _install_stubs(
    monkeypatch: pytest.MonkeyPatch,
    *,
    pair_mgr: _StubPairManager,
    forward: _StubForwardBind,
) -> None:
    """Wire stubs into the modules the supervisor imports lazily."""
    # Skip the 15 s settle delay and any RETRY_BACKOFF_S sleeps.
    monkeypatch.setattr(auto_pair_mod, "START_DELAY_S", 0.0)
    monkeypatch.setattr(auto_pair_mod, "RETRY_BACKOFF_S", 0.0)

    # load_config / pair_manager / detect_wfb_adapters / forward_start_bind
    # are imported inside _run; install module-level stubs that the
    # ``from … import …`` lookup will resolve to.
    import ados.core.config as cfg_mod
    import ados.services.ground_station.pair_manager as pm_mod
    import ados.services.wfb.adapter as adapter_mod
    import ados.services.wfb.bind_client as bind_client_mod

    monkeypatch.setattr(cfg_mod, "load_config", lambda: _StubConfig())
    monkeypatch.setattr(pm_mod, "get_pair_manager", lambda: pair_mgr)
    monkeypatch.setattr(adapter_mod, "detect_wfb_adapters", lambda: [_FakeAdapter()])
    monkeypatch.setattr(bind_client_mod, "forward_start_bind", forward)


def _run_supervisor(role: str = "drone", timeout: float = 5.0) -> AutoPairSupervisor:
    """Drive the supervisor's _run() to completion within ``timeout``."""
    sup = AutoPairSupervisor(role=role)

    async def _drive() -> None:
        await asyncio.wait_for(sup._run(), timeout=timeout)

    asyncio.run(_drive())
    return sup


@pytest.fixture
def sidecar(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    path = tmp_path / "wfb_failover.json"
    monkeypatch.setattr(auto_pair_mod, "FAILOVER_STATE_PATH", path)
    return path


def test_failover_triggers_after_max_attempts(
    monkeypatch: pytest.MonkeyPatch,
    sidecar: Path,
) -> None:
    """N consecutive failed attempts flip the sidecar to cloud_relay."""
    pair_mgr = _StubPairManager()
    # Every attempt returns a non-paired terminal state.
    forward = _StubForwardBind(
        [{"state": "failed", "error": "no peer"}] * (MAX_LOCAL_BIND_ATTEMPTS + 5)
    )
    _install_stubs(monkeypatch, pair_mgr=pair_mgr, forward=forward)

    sup = _run_supervisor()

    # The supervisor stops once it gives up.
    assert forward.call_count == MAX_LOCAL_BIND_ATTEMPTS
    assert sup.failover_state == "cloud_relay"
    assert sidecar.exists()
    body = json.loads(sidecar.read_text())
    assert body == {"state": "cloud_relay"}


def test_failover_stays_local_when_pair_succeeds_within_cap(
    monkeypatch: pytest.MonkeyPatch,
    sidecar: Path,
) -> None:
    """Nine failures followed by a successful pair keeps the state at local."""
    pair_mgr = _StubPairManager()
    script: list[Any] = [
        {"state": "failed", "error": "no peer"} for _ in range(MAX_LOCAL_BIND_ATTEMPTS - 1)
    ]
    script.append({"state": "paired", "fingerprint": "abc"})
    forward = _StubForwardBind(script)
    _install_stubs(monkeypatch, pair_mgr=pair_mgr, forward=forward)

    sup = _run_supervisor()

    assert forward.call_count == MAX_LOCAL_BIND_ATTEMPTS  # 9 failed + 1 success
    assert sup.failover_state == "local"
    body = json.loads(sidecar.read_text())
    assert body == {"state": "local"}


def test_run_resets_failover_state_at_start(
    monkeypatch: pytest.MonkeyPatch,
    sidecar: Path,
) -> None:
    """A fresh _run() resets the sidecar to ``local`` before retrying."""
    # Pre-seed the sidecar with a stale cloud_relay state.
    sidecar.parent.mkdir(parents=True, exist_ok=True)
    sidecar.write_text(json.dumps({"state": "cloud_relay"}))

    pair_mgr = _StubPairManager()
    # First attempt succeeds so we observe the initial reset only.
    forward = _StubForwardBind([{"state": "paired", "fingerprint": "abc"}])
    _install_stubs(monkeypatch, pair_mgr=pair_mgr, forward=forward)

    sup = _run_supervisor()

    assert sup.failover_state == "local"
    body = json.loads(sidecar.read_text())
    assert body == {"state": "local"}


def test_persist_failover_state_writes_atomically(
    monkeypatch: pytest.MonkeyPatch,
    sidecar: Path,
) -> None:
    """The helper writes via the atomic JSON helper (no partial files left)."""
    sup = AutoPairSupervisor(role="drone")
    sup._persist_failover_state("cloud_relay")

    # The sidecar exists and has only the final content; no .tmp leftovers.
    assert sidecar.exists()
    body = json.loads(sidecar.read_text())
    assert body == {"state": "cloud_relay"}
    siblings = list(sidecar.parent.glob("wfb_failover.json.*"))
    assert siblings == [], f"stray temp files: {siblings}"
