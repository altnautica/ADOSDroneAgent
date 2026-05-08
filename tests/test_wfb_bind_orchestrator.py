"""Tests for the bind orchestrator.

Real systemctl, real socat, and the real wfb-ng binaries are not
available inside the test environment — these tests cover the state
machine, the busy-lock 409 behavior, and the role wiring without
firing actual subprocesses.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from ados.services.wfb import bind_orchestrator as orch_mod
from ados.services.wfb.bind_orchestrator import (
    BindBusyError,
    BindError,
    BindState,
    get_bind_orchestrator,
)


@pytest.fixture(autouse=True)
def _reset_orchestrator() -> None:
    orch_mod._reset_for_tests()
    yield
    orch_mod._reset_for_tests()


def test_status_idle_returns_none() -> None:
    orch = get_bind_orchestrator()
    assert asyncio.run(orch.status()) is None


def test_start_local_bind_fails_without_upstream_artifacts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """With /etc/bind.key + /etc/bind.yaml absent, the orchestrator must
    bail BEFORE touching systemd or socat."""
    missing_key = tmp_path / "no" / "bind.key"
    missing_yaml = tmp_path / "no" / "bind.yaml"
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", missing_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", missing_yaml)
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))

    orch = get_bind_orchestrator()
    result = asyncio.run(orch.start_local_bind(role="gs"))
    assert result["state"] == BindState.FAILED.value
    assert "missing" in (result["error"] or "")


def test_start_local_bind_busy_raises(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Concurrent calls fail-fast with BindBusyError so the REST surface
    can return 409."""
    bind_key = tmp_path / "bind.key"
    bind_yaml = tmp_path / "bind.yaml"
    bind_key.write_bytes(b"\x00")
    bind_yaml.write_bytes(b"")
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", bind_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", bind_yaml)
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))

    orch = get_bind_orchestrator()

    # Hold the orchestrator's internal lock by calling _run_session
    # behind an awaitable that never returns.
    held = asyncio.Event()
    finish = asyncio.Event()

    async def _hold_lock() -> None:
        async with orch._lock:  # type: ignore[attr-defined]
            held.set()
            await finish.wait()

    async def _scenario() -> None:
        bg = asyncio.create_task(_hold_lock())
        await held.wait()
        with pytest.raises(BindBusyError):
            await orch.start_local_bind(role="gs", timeout_s=1.0)
        finish.set()
        await bg

    asyncio.run(_scenario())


def test_start_local_bind_drone_path_calls_pair_manager(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """End-to-end happy path on the drone side: server runs, drone.key
    appears, PairManager.apply_keypair is invoked with the bytes."""
    bind_key = tmp_path / "bind.key"
    bind_yaml = tmp_path / "bind.yaml"
    bind_key.write_bytes(b"\x00")
    bind_yaml.write_bytes(b"")
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", bind_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", bind_yaml)

    drone_key_path = tmp_path / "drone.key"
    monkeypatch.setattr(orch_mod, "UPSTREAM_DRONE_KEY", drone_key_path)
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_SERVER_SH", str(tmp_path / "wfb_bind_server.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")

    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))

    async def _wait_for_iface_stub(iface: str, timeout_s: float = 0.0) -> bool:
        return True

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    # Stub out the actual socat exec so it returns rc=0 and writes the
    # drone.key like the upstream server would have done.
    async def _fake_subprocess_exec(*cmd, **_kw):
        proc = AsyncMock()
        drone_key_path.write_bytes(b"\x42" * 32 + b"\x99" * 32)

        async def _communicate():
            return (b"", b"")

        proc.communicate = _communicate
        proc.returncode = 0
        return proc

    apply_calls: list = []

    async def fake_apply(blob, role, peer_id):  # noqa: ARG001
        apply_calls.append((blob, role, peer_id))
        return {
            "paired": True,
            "fingerprint": "deadbeefcafefeed",
            "role": role,
            "paired_with_device_id": peer_id,
            "paired_at": "2026-01-01T00:00:00+00:00",
        }

    with patch(
        "asyncio.create_subprocess_exec", side_effect=_fake_subprocess_exec
    ):
        from ados.services.ground_station import pair_manager as pm_mod

        pm_mod._reset_for_tests()
        with patch.object(
            pm_mod.PairManager,
            "apply_keypair",
            side_effect=fake_apply,
        ):
            orch = get_bind_orchestrator()
            result = asyncio.run(
                orch.start_local_bind(
                    role="drone",
                    peer_device_id="gs-1",
                    timeout_s=2.0,
                )
            )

    assert result["state"] == BindState.PAIRED.value
    assert len(apply_calls) == 1
    blob, role, peer_id = apply_calls[0]
    assert role == "drone"
    assert peer_id == "gs-1"
    assert len(blob) == 64
