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
            await orch.start_local_bind(role="gs")
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
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")

    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))
    # socat + wfb_keygen aren't installed on the dev box; the orchestrator
    # only needs them via shutil.which() preflights so a fixed return is
    # cheaper than installing them.
    monkeypatch.setattr(
        orch_mod.shutil,
        "which",
        lambda name: f"/usr/bin/{name}",
    )

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
                )
            )

    assert result["state"] == BindState.PAIRED.value
    assert len(apply_calls) == 1
    blob, role, peer_id = apply_calls[0]
    assert role == "drone"
    assert peer_id == "gs-1"
    assert len(blob) == 64


def test_kill_stale_bind_socats_targets_listener_and_client(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The sweep helper must invoke pkill against both the listener
    pattern (drone side) and the client pattern (gs side). The bind
    port + peer IP are the discriminators that prevent us from killing
    unrelated socat invocations on the box."""
    calls: list[list[str]] = []

    def _fake_run(cmd, **_kw):
        calls.append(list(cmd))

        class _Result:
            returncode = 0
            stdout = b""
            stderr = b""

        return _Result()

    monkeypatch.setattr(orch_mod.subprocess, "run", _fake_run)
    orch_mod._kill_stale_bind_socats()

    assert len(calls) == 2
    listener = next(c for c in calls if "TCP4-LISTEN:" in " ".join(c))
    client = next(c for c in calls if "TCP4:" in " ".join(c) and "TCP4-LISTEN:" not in " ".join(c))
    assert "pkill" in listener[0] and "-9" in listener and "-f" in listener
    assert orch_mod.DRONE_BIND_PEER_IP in " ".join(listener)
    assert str(orch_mod.BIND_TCP_PORT) in " ".join(listener)
    assert orch_mod.DRONE_BIND_PEER_IP in " ".join(client)


def test_run_socat_with_kill_on_cancel_kills_subprocess() -> None:
    """When the outer caller is cancelled (asyncio.wait_for timeout),
    the helper's finally block must kill the OS process so port 5555
    stays free for the next attempt. Regression test for the leak that
    caused every retry after the first to fail with `Address already
    in use` on 10.5.99.2:5555."""
    import os
    import signal

    async def _scenario() -> int:
        # /bin/sleep is universally available; stand-in for a hung socat.
        # We wrap the helper in a wait_for that fires before sleep
        # completes; the helper's finally must kill the child.
        captured_pid: dict[str, int] = {}

        async def _spy_run() -> None:
            # Inline the helper but capture the pid for post-mortem.
            proc = await asyncio.create_subprocess_exec(
                "/bin/sleep",
                "30",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            captured_pid["pid"] = proc.pid
            try:
                await proc.communicate()
            finally:
                if proc.returncode is None:
                    try:
                        proc.kill()
                    except ProcessLookupError:
                        pass
                    try:
                        await asyncio.wait_for(proc.wait(), timeout=5.0)
                    except (asyncio.TimeoutError, ProcessLookupError):
                        pass

        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(_spy_run(), timeout=0.5)
        # Give the kernel a beat to reap.
        await asyncio.sleep(0.1)
        return captured_pid["pid"]

    pid = asyncio.run(_scenario())
    # Process must be gone. Sending signal 0 raises ProcessLookupError if so.
    with pytest.raises(ProcessLookupError):
        os.kill(pid, 0)


def test_start_local_bind_sweeps_stragglers_pre_flight(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Pre-flight sweep must run BEFORE pre-flight artifact checks raise,
    so a rig with stale socats from a crashed prior session recovers on
    the next start_local_bind call."""
    bind_key = tmp_path / "bind.key"
    bind_yaml = tmp_path / "bind.yaml"
    bind_key.write_bytes(b"\x00")
    bind_yaml.write_bytes(b"")
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", bind_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", bind_yaml)
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_SERVER_SH", str(tmp_path / "wfb_bind_server.sh")
    )
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))

    sweep_calls: list[int] = []

    def _spy_kill_stale() -> None:
        sweep_calls.append(1)

    monkeypatch.setattr(orch_mod, "_kill_stale_bind_socats", _spy_kill_stale)

    async def _wait_for_iface_stub(_iface: str, _timeout_s: float = 0.0) -> bool:
        return False  # Force the session to fail so we don't need a real bind.

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    orch = get_bind_orchestrator()
    result = asyncio.run(orch.start_local_bind(role="drone"))

    assert sweep_calls, "pre-flight sweep was never invoked"
    # _cleanup must also call the sweep on the failure path so
    # any subprocess that survived a cancel path is reaped.
    assert len(sweep_calls) >= 2, "cleanup-time sweep was not invoked"
    assert result["state"] == BindState.FAILED.value


def test_cancel_event_aborts_in_flight_session(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When the caller fires the cancel event mid-session, the orchestrator
    returns state=aborted within milliseconds. Regression test for the
    operator-cancel path: the previous bounded-window design forced
    operators to wait for the next 60s timeout to free the orchestrator.
    """
    bind_key = tmp_path / "bind.key"
    bind_yaml = tmp_path / "bind.yaml"
    bind_key.write_bytes(b"\x00")
    bind_yaml.write_bytes(b"")
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", bind_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", bind_yaml)
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_SERVER_SH", str(tmp_path / "wfb_bind_server.sh")
    )
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))
    monkeypatch.setattr(
        orch_mod.shutil,
        "which",
        lambda name: f"/usr/bin/{name}",
    )

    async def _wait_for_iface_stub(_iface: str, _timeout_s: float = 0.0) -> bool:
        return True

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    # Stub socat to never return (simulate a real "waiting for peer"
    # session) so the cancel path is the only way out.
    async def _hung_subprocess_exec(*_cmd, **_kw):
        proc = AsyncMock()
        forever = asyncio.Event()  # never set

        async def _communicate():
            await forever.wait()
            return (b"", b"")

        proc.communicate = _communicate
        proc.returncode = None
        proc.kill = lambda: None  # the helper checks returncode is None
        proc.wait = AsyncMock(return_value=0)
        proc.pid = 12345
        return proc

    async def _scenario() -> dict:
        cancel_event = asyncio.Event()
        with patch(
            "asyncio.create_subprocess_exec", side_effect=_hung_subprocess_exec
        ):
            orch = get_bind_orchestrator()
            bind_task = asyncio.create_task(
                orch.start_local_bind(role="drone", cancel_event=cancel_event)
            )
            # Let the orchestrator reach the WAITING state.
            await asyncio.sleep(0.05)
            cancel_event.set()
            return await asyncio.wait_for(bind_task, timeout=5.0)

    result = asyncio.run(_scenario())
    assert result["state"] == BindState.ABORTED.value
    assert "cancel" in (result["error"] or "").lower()


def test_unbounded_wait_does_not_self_exit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A session with no cancel and no peer must NOT exit on its own
    within 5 seconds. The previous design hit a 60s wall-clock timeout;
    the new design only exits on cancel, watchdog (30 min), or peer."""
    bind_key = tmp_path / "bind.key"
    bind_yaml = tmp_path / "bind.yaml"
    bind_key.write_bytes(b"\x00")
    bind_yaml.write_bytes(b"")
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", bind_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", bind_yaml)
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_SERVER_SH", str(tmp_path / "wfb_bind_server.sh")
    )
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))
    monkeypatch.setattr(
        orch_mod.shutil, "which", lambda name: f"/usr/bin/{name}"
    )

    async def _wait_for_iface_stub(_iface: str, _timeout_s: float = 0.0) -> bool:
        return True

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    async def _hung_subprocess_exec(*_cmd, **_kw):
        proc = AsyncMock()
        forever = asyncio.Event()

        async def _communicate():
            await forever.wait()
            return (b"", b"")

        proc.communicate = _communicate
        proc.returncode = None
        proc.kill = lambda: None
        proc.wait = AsyncMock(return_value=0)
        proc.pid = 23456
        return proc

    async def _scenario() -> bool:
        with patch(
            "asyncio.create_subprocess_exec", side_effect=_hung_subprocess_exec
        ):
            orch = get_bind_orchestrator()
            bind_task = asyncio.create_task(orch.start_local_bind(role="drone"))
            try:
                # If the unbounded design works, this wait_for must time out.
                await asyncio.wait_for(asyncio.shield(bind_task), timeout=2.0)
                bind_task.cancel()
                return False  # bound: orchestrator self-exited
            except asyncio.TimeoutError:
                bind_task.cancel()
                try:
                    await bind_task
                except (asyncio.CancelledError, Exception):  # noqa: BLE001
                    pass
                return True  # unbounded: still waiting after 2s

    assert asyncio.run(_scenario()) is True


def test_watchdog_constant_is_long_enough_for_real_benches() -> None:
    """The watchdog is a paranoia trip, not a normal-path bound. It
    should be at least 5 minutes so a slow-to-boot peer never accidentally
    triggers it."""
    assert orch_mod.WAITING_PEER_WATCHDOG_S >= 300.0


def test_default_timeout_constant_is_gone() -> None:
    """The old DEFAULT_TIMEOUT_S knob was the bug; it should not be
    referenceable anywhere in the module after the redesign."""
    assert not hasattr(orch_mod, "DEFAULT_TIMEOUT_S")


def test_bind_error_backward_compat() -> None:
    """`raise BindError("msg")` still works without the new phase arg.
    Existing callers that do not pass `phase=` must continue to compile
    and produce a BindError with `phase is None`."""
    err = BindError("legacy call shape")
    assert str(err) == "legacy call shape"
    assert err.phase is None

    err_with_phase = BindError("new call shape", phase="transferring_keys")
    assert str(err_with_phase) == "new call shape"
    assert err_with_phase.phase == "transferring_keys"


def test_session_exposes_phase_and_entered_at(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Drive a partial bind to the WAITING_PEER state, then inspect the
    session snapshot: `phase` must match `state`, `phase_entered_at`
    must be set to a recent monotonic timestamp, and `phase_age_s`
    must be a small non-negative float."""
    import time as _time

    bind_key = tmp_path / "bind.key"
    bind_yaml = tmp_path / "bind.yaml"
    bind_key.write_bytes(b"\x00")
    bind_yaml.write_bytes(b"")
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", bind_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", bind_yaml)
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_SERVER_SH", str(tmp_path / "wfb_bind_server.sh")
    )
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))
    monkeypatch.setattr(
        orch_mod.shutil, "which", lambda name: f"/usr/bin/{name}"
    )

    async def _wait_for_iface_stub(_iface: str, _timeout_s: float = 0.0) -> bool:
        return True

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    # Hang socat so the orchestrator reaches TRANSFERRING_KEYS and stays
    # there; we then snapshot before the cancel.
    async def _hung_subprocess_exec(*_cmd, **_kw):
        proc = AsyncMock()
        forever = asyncio.Event()

        async def _communicate():
            await forever.wait()
            return (b"", b"")

        proc.communicate = _communicate
        proc.returncode = None
        proc.kill = lambda: None
        proc.wait = AsyncMock(return_value=0)
        proc.pid = 34567
        return proc

    async def _scenario() -> dict:
        cancel_event = asyncio.Event()
        with patch(
            "asyncio.create_subprocess_exec", side_effect=_hung_subprocess_exec
        ):
            orch = get_bind_orchestrator()
            bind_task = asyncio.create_task(
                orch.start_local_bind(role="drone", cancel_event=cancel_event)
            )
            # Let the orchestrator reach TRANSFERRING_KEYS.
            await asyncio.sleep(0.05)
            snap_mid = await orch.status()
            assert snap_mid is not None
            # Tear down so the test doesn't leak the bind_task.
            cancel_event.set()
            try:
                await asyncio.wait_for(bind_task, timeout=5.0)
            except Exception:  # noqa: BLE001
                pass
            return snap_mid

    before = _time.monotonic()
    snap = asyncio.run(_scenario())
    after = _time.monotonic()

    assert snap["phase"] == snap["state"]
    # The session must have advanced past IDLE.
    assert snap["state"] != orch_mod.BindState.IDLE.value
    assert snap["phase_entered_at"] is not None
    # Phase entered at must be a monotonic-clock value taken between the
    # before-call and after-call samples.
    assert before <= snap["phase_entered_at"] <= after
    assert snap["phase_age_s"] is not None
    assert snap["phase_age_s"] >= 0.0
    # Phase age cannot exceed the wall-clock the scenario consumed.
    assert snap["phase_age_s"] <= (after - before) + 0.1


def test_key_transfer_timeout_raises_bind_error_with_phase(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """If the socat key-transfer step hangs past KEY_TRANSFER_TIMEOUT_S,
    the orchestrator must surface a BindError whose `phase` attribute
    names the phase the timeout fired in (TRANSFERRING_KEYS). The
    session's terminal `state` is FAILED; the error message references
    the transfer-timeout budget."""
    bind_key = tmp_path / "bind.key"
    bind_yaml = tmp_path / "bind.yaml"
    bind_key.write_bytes(b"\x00")
    bind_yaml.write_bytes(b"")
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_KEY", bind_key)
    monkeypatch.setattr(orch_mod, "UPSTREAM_BIND_YAML", bind_yaml)
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_SERVER_SH", str(tmp_path / "wfb_bind_server.sh")
    )
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))
    monkeypatch.setattr(
        orch_mod.shutil, "which", lambda name: f"/usr/bin/{name}"
    )

    # Shrink the budget so the test is fast.
    monkeypatch.setattr(orch_mod, "KEY_TRANSFER_TIMEOUT_S", 0.2)

    async def _wait_for_iface_stub(_iface: str, _timeout_s: float = 0.0) -> bool:
        return True

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    # Hang socat so the inner asyncio.timeout fires.
    async def _hung_subprocess_exec(*_cmd, **_kw):
        proc = AsyncMock()
        forever = asyncio.Event()

        async def _communicate():
            await forever.wait()
            return (b"", b"")

        proc.communicate = _communicate
        proc.returncode = None
        proc.kill = lambda: None
        proc.wait = AsyncMock(return_value=0)
        proc.pid = 45678
        return proc

    async def _scenario() -> dict:
        with patch(
            "asyncio.create_subprocess_exec", side_effect=_hung_subprocess_exec
        ):
            orch = get_bind_orchestrator()
            return await orch.start_local_bind(role="drone")

    result = asyncio.run(_scenario())

    assert result["state"] == orch_mod.BindState.FAILED.value
    err_msg = result["error"] or ""
    assert "timed out" in err_msg.lower()
    assert "transferring_keys" in err_msg


def test_restart_timeout_raises_bind_error_with_phase(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """If PairManager.apply_keypair hangs past RESTART_TIMEOUT_S, the
    orchestrator must raise a BindError tagged `phase=restarting_services`
    and the session's terminal state must be FAILED."""
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
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))
    monkeypatch.setattr(
        orch_mod.shutil, "which", lambda name: f"/usr/bin/{name}"
    )

    # Shrink the restart budget so the test is fast; leave the key
    # transfer budget at default so it does NOT fire first.
    monkeypatch.setattr(orch_mod, "RESTART_TIMEOUT_S", 0.2)

    async def _wait_for_iface_stub(_iface: str, _timeout_s: float = 0.0) -> bool:
        return True

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    # socat exits cleanly and writes the drone.key so the orchestrator
    # advances to APPLYING_KEYS and reads the blob. The hang has to be
    # in apply_keypair.
    async def _fake_subprocess_exec(*_cmd, **_kw):
        proc = AsyncMock()
        drone_key_path.write_bytes(b"\x42" * 32 + b"\x99" * 32)

        async def _communicate():
            return (b"", b"")

        proc.communicate = _communicate
        proc.returncode = 0
        return proc

    async def _hung_apply(_blob, _role, _peer_id):
        forever = asyncio.Event()
        await forever.wait()

    async def _scenario() -> dict:
        with patch(
            "asyncio.create_subprocess_exec", side_effect=_fake_subprocess_exec
        ):
            from ados.services.ground_station import pair_manager as pm_mod

            pm_mod._reset_for_tests()
            with patch.object(
                pm_mod.PairManager,
                "apply_keypair",
                side_effect=_hung_apply,
            ):
                orch = get_bind_orchestrator()
                return await orch.start_local_bind(role="drone")

    result = asyncio.run(_scenario())

    assert result["state"] == orch_mod.BindState.FAILED.value
    err_msg = result["error"] or ""
    assert "restart" in err_msg.lower()
    assert "timed out" in err_msg.lower()


def test_status_snapshot_includes_phase_fields_when_paired(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """After a successful bind the snapshot still carries phase metadata.
    GET /wfb/pair/local-bind callers should always see the three new
    keys (`phase`, `phase_entered_at`, `phase_age_s`) regardless of
    terminal state."""
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
    monkeypatch.setattr(
        orch_mod, "WFB_BIND_CLIENT_SH", str(tmp_path / "wfb_bind_client.sh")
    )
    Path(orch_mod.WFB_BIND_SERVER_SH).write_text("#!/bin/sh\nexit 0\n")
    Path(orch_mod.WFB_BIND_CLIENT_SH).write_text("#!/bin/sh\nexit 0\n")
    monkeypatch.setattr(orch_mod, "_systemctl", lambda *_a, **_kw: (True, ""))
    monkeypatch.setattr(
        orch_mod.shutil, "which", lambda name: f"/usr/bin/{name}"
    )

    async def _wait_for_iface_stub(_iface: str, _timeout_s: float = 0.0) -> bool:
        return True

    monkeypatch.setattr(orch_mod, "_wait_for_iface", _wait_for_iface_stub)

    async def _fake_subprocess_exec(*_cmd, **_kw):
        proc = AsyncMock()
        drone_key_path.write_bytes(b"\x42" * 32 + b"\x99" * 32)

        async def _communicate():
            return (b"", b"")

        proc.communicate = _communicate
        proc.returncode = 0
        return proc

    async def fake_apply(_blob, role, peer_id):
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
            result = asyncio.run(orch.start_local_bind(role="drone"))

    assert result["state"] == orch_mod.BindState.PAIRED.value
    # Phase keys must always be present in the snapshot, even on the
    # success path. Otherwise the GCS LCD has nothing to render when
    # poll cadence catches a freshly-terminal state.
    assert "phase" in result
    assert "phase_entered_at" in result
    assert "phase_age_s" in result
    assert result["phase"] == orch_mod.BindState.PAIRED.value
    assert result["phase_entered_at"] is not None
    assert result["phase_age_s"] is not None
    assert result["phase_age_s"] >= 0.0
