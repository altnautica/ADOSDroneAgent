"""Tests for the HDMI kiosk service.

The kiosk service is supervisory: it probes the DRM card node, resolves
a target URL (config / env / default), and spawns ``cage -- chromium-browser``
under supervision. Tests here cover the pure helpers (low-RAM threshold,
URL resolution, argv build, HDMI probe) plus the supervisor lifecycle
contract via fake asyncio subprocesses. No real chromium or cage binary
is invoked.
"""

from __future__ import annotations

import asyncio
from types import SimpleNamespace
from typing import Any
from unittest.mock import MagicMock, patch

import pytest

from ados.services.kiosk import kiosk_service as ks
from ados.services.kiosk.kiosk_service import (
    KioskSupervisor,
    _build_chromium_argv,
    _get_kiosk_config,
    _hdmi_present,
    _low_ram_board,
    _resolve_target_url,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _config_with_kiosk(url: str | None = None, minimal: bool | None = None) -> SimpleNamespace:
    """Build a duck-typed config that mimics ``config.ground_station.kiosk``."""
    return SimpleNamespace(
        ground_station=SimpleNamespace(
            kiosk=SimpleNamespace(target_url=url, minimal_layer=minimal)
        )
    )


class _FakeProc:
    """Minimal stand-in for ``asyncio.subprocess.Process`` used by the supervisor."""

    def __init__(
        self,
        *,
        wait_result: int = 0,
        wait_delay: float = 0.0,
        sigterm_honored: bool = True,
        stderr: bytes = b"",
    ) -> None:
        self.pid = 4242
        self.returncode: int | None = None
        self._wait_result = wait_result
        self._wait_delay = wait_delay
        self._sigterm_honored = sigterm_honored
        self._terminate_called = False
        self._kill_called = False
        self._exit_event = asyncio.Event()
        # Pre-set the exit if no delay is requested so wait() resolves immediately.
        if wait_delay <= 0:
            self._schedule_exit()

        async def _read_stderr() -> bytes:
            return stderr

        self.stderr = SimpleNamespace(read=_read_stderr)

    def _schedule_exit(self) -> None:
        self.returncode = self._wait_result
        self._exit_event.set()

    async def wait(self) -> int:
        if not self._exit_event.is_set():
            if self._wait_delay > 0:
                try:
                    await asyncio.wait_for(self._exit_event.wait(), timeout=self._wait_delay)
                except TimeoutError:
                    pass
            if not self._exit_event.is_set():
                # Fall through and resolve so wait_for in the supervisor returns.
                self._schedule_exit()
        return self.returncode or 0

    def terminate(self) -> None:
        self._terminate_called = True
        if self._sigterm_honored:
            self._schedule_exit()

    def kill(self) -> None:
        self._kill_called = True
        self.returncode = -9
        self._exit_event.set()


# ---------------------------------------------------------------------------
# HDMI probe + config accessor
# ---------------------------------------------------------------------------


def test_hdmi_present_true_when_path_exists() -> None:
    with patch.object(ks, "_DRM_CARD_PATH") as path:
        path.exists.return_value = True
        assert _hdmi_present() is True


def test_hdmi_present_false_when_path_missing() -> None:
    with patch.object(ks, "_DRM_CARD_PATH") as path:
        path.exists.return_value = False
        assert _hdmi_present() is False


def test_get_kiosk_config_missing_section_returns_nones() -> None:
    """Old config shape (no ``ground_station``) yields (None, None)."""
    assert _get_kiosk_config(SimpleNamespace()) == (None, None)


def test_get_kiosk_config_empty_kiosk_block_returns_nones() -> None:
    cfg = SimpleNamespace(ground_station=SimpleNamespace(kiosk=None))
    assert _get_kiosk_config(cfg) == (None, None)


def test_get_kiosk_config_blank_url_normalized_to_none() -> None:
    """Whitespace-only URLs are treated as unset."""
    cfg = _config_with_kiosk(url="   ", minimal=True)
    url, minimal = _get_kiosk_config(cfg)
    assert url is None
    assert minimal is True


def test_get_kiosk_config_returns_configured_values() -> None:
    cfg = _config_with_kiosk(url="http://example/hud", minimal=False)
    assert _get_kiosk_config(cfg) == ("http://example/hud", False)


# ---------------------------------------------------------------------------
# Low-RAM threshold (board with <3 GiB total RAM auto-trips minimal layer)
# ---------------------------------------------------------------------------


def test_low_ram_board_true_when_under_threshold() -> None:
    """A 2 GiB board falls under the 3 GiB threshold."""
    fake_psutil = MagicMock()
    fake_psutil.virtual_memory.return_value = MagicMock(total=2 * 1024 * 1024 * 1024)
    with patch.dict("sys.modules", {"psutil": fake_psutil}):
        assert _low_ram_board() is True


def test_low_ram_board_false_when_at_or_above_threshold() -> None:
    """A 4 GiB board is comfortably above the threshold."""
    fake_psutil = MagicMock()
    fake_psutil.virtual_memory.return_value = MagicMock(total=4 * 1024 * 1024 * 1024)
    with patch.dict("sys.modules", {"psutil": fake_psutil}):
        assert _low_ram_board() is False


def test_low_ram_board_threshold_boundary() -> None:
    """At exactly 3 GiB the helper returns False (strict less-than)."""
    fake_psutil = MagicMock()
    fake_psutil.virtual_memory.return_value = MagicMock(total=3 * 1024 * 1024 * 1024)
    with patch.dict("sys.modules", {"psutil": fake_psutil}):
        assert _low_ram_board() is False


def test_low_ram_board_psutil_failure_returns_false() -> None:
    """psutil.virtual_memory() exceptions degrade safely to ``False``."""
    fake_psutil = MagicMock()
    fake_psutil.virtual_memory.side_effect = RuntimeError("no /proc on this box")
    with patch.dict("sys.modules", {"psutil": fake_psutil}):
        assert _low_ram_board() is False


# ---------------------------------------------------------------------------
# URL resolution
# ---------------------------------------------------------------------------


def test_resolve_target_url_defaults_when_nothing_set(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("ADOS_KIOSK_URL", raising=False)
    monkeypatch.delenv("ADOS_KIOSK_MINIMAL_LAYER", raising=False)
    with patch.object(ks, "_low_ram_board", return_value=False):
        url, minimal = _resolve_target_url(SimpleNamespace())
    assert url == "http://localhost:8080/cockpit"
    assert minimal is False


def test_resolve_target_url_env_override_used(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("ADOS_KIOSK_URL", "http://env-host/hud")
    monkeypatch.delenv("ADOS_KIOSK_MINIMAL_LAYER", raising=False)
    with patch.object(ks, "_low_ram_board", return_value=False):
        url, minimal = _resolve_target_url(SimpleNamespace())
    assert url == "http://env-host/hud"
    assert minimal is False


def test_resolve_target_url_config_beats_env(monkeypatch: pytest.MonkeyPatch) -> None:
    """Config-supplied URL wins over the env override."""
    monkeypatch.setenv("ADOS_KIOSK_URL", "http://env-host/hud")
    monkeypatch.delenv("ADOS_KIOSK_MINIMAL_LAYER", raising=False)
    cfg = _config_with_kiosk(url="http://cfg-host/hud")
    with patch.object(ks, "_low_ram_board", return_value=False):
        url, _ = _resolve_target_url(cfg)
    assert url == "http://cfg-host/hud"


def test_resolve_target_url_low_ram_appends_minimal_layer(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("ADOS_KIOSK_URL", raising=False)
    monkeypatch.delenv("ADOS_KIOSK_MINIMAL_LAYER", raising=False)
    with patch.object(ks, "_low_ram_board", return_value=True):
        url, minimal = _resolve_target_url(SimpleNamespace())
    assert minimal is True
    assert url.endswith("?layer=minimal")


def test_resolve_target_url_config_minimal_flag_appends_query(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("ADOS_KIOSK_URL", raising=False)
    monkeypatch.delenv("ADOS_KIOSK_MINIMAL_LAYER", raising=False)
    cfg = _config_with_kiosk(url="http://host/hud?theme=dark", minimal=True)
    with patch.object(ks, "_low_ram_board", return_value=False):
        url, minimal = _resolve_target_url(cfg)
    assert minimal is True
    # Existing query separator preserved.
    assert url == "http://host/hud?theme=dark&layer=minimal"


def test_resolve_target_url_env_can_force_full_layer(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """``ADOS_KIOSK_MINIMAL_LAYER=0`` overrides a low-RAM detection."""
    monkeypatch.setenv("ADOS_KIOSK_MINIMAL_LAYER", "0")
    monkeypatch.delenv("ADOS_KIOSK_URL", raising=False)
    with patch.object(ks, "_low_ram_board", return_value=True):
        url, minimal = _resolve_target_url(SimpleNamespace())
    assert minimal is False
    assert "layer=minimal" not in url


# ---------------------------------------------------------------------------
# Chromium argv
# ---------------------------------------------------------------------------


def test_build_chromium_argv_invokes_cage_and_resolved_browser() -> None:
    # The browser binary is resolved at runtime (its name varies by distro), so the
    # third argv slot is whatever `_resolve_browser_binary` returns.
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        argv = _build_chromium_argv("http://target/hud")
    assert argv[0] == "cage"
    assert argv[1] == "--"
    assert argv[2] == "/usr/bin/chromium"
    assert "--kiosk" in argv
    assert "--ozone-platform=wayland" in argv
    assert argv[-1] == "http://target/hud"


def test_resolve_browser_binary_returns_first_found() -> None:
    """`shutil.which` resolves the first present candidate to its path."""

    def fake_which(name: str) -> str | None:
        # Only `chromium` (the second candidate) is installed on this box.
        return "/usr/bin/chromium" if name == "chromium" else None

    with patch("shutil.which", side_effect=fake_which):
        assert ks._resolve_browser_binary() == "/usr/bin/chromium"


def test_resolve_browser_binary_raises_naming_every_candidate() -> None:
    """No browser on PATH → FileNotFoundError that names every tried candidate."""
    with patch("shutil.which", return_value=None):
        with pytest.raises(FileNotFoundError) as exc:
            ks._resolve_browser_binary()
    msg = str(exc.value)
    for name in ks._BROWSER_CANDIDATES:
        assert name in msg


# ---------------------------------------------------------------------------
# Adaptive launch: run inside a live desktop vs own the display via cage
# ---------------------------------------------------------------------------


def test_windowed_argv_wayland_has_no_cage_and_wayland_platform() -> None:
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        argv = ks._build_windowed_chromium_argv("http://target/cockpit", "wayland")
    assert "cage" not in argv
    assert argv[0] == "/usr/bin/chromium"
    assert "--kiosk" in argv
    assert "--ozone-platform=wayland" in argv
    assert argv[-1] == "http://target/cockpit"


def test_windowed_argv_x11_uses_x11_platform() -> None:
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        argv = ks._build_windowed_chromium_argv("http://target/cockpit", "x11")
    assert "--ozone-platform=x11" in argv
    assert "cage" not in argv


def test_detect_desktop_session_returns_active_wayland_session() -> None:
    props = {
        "Type": "wayland",
        "State": "active",
        "Active": "yes",
        "Remote": "no",
        "User": "1000",
        "Display": "",
    }
    with patch.object(ks, "_loginctl_sessions", return_value=["c1"]):
        with patch.object(ks, "_loginctl_session_props", return_value=props):
            session = ks._detect_desktop_session()
    assert session is not None
    assert session.session_type == "wayland"
    assert session.uid == 1000


def test_detect_desktop_session_returns_x11_session_with_display() -> None:
    props = {
        "Type": "x11",
        "Active": "yes",
        "Remote": "no",
        "User": "1000",
        "Display": ":0",
    }
    with patch.object(ks, "_loginctl_sessions", return_value=["c1"]):
        with patch.object(ks, "_loginctl_session_props", return_value=props):
            session = ks._detect_desktop_session()
    assert session is not None
    assert session.session_type == "x11"
    assert session.display == ":0"


def test_detect_desktop_session_none_when_only_tty_sessions() -> None:
    """A bare tty (non-graphical) session must not be treated as a desktop."""
    props = {"Type": "tty", "Active": "yes", "Remote": "no", "User": "1000"}
    with patch.object(ks, "_loginctl_sessions", return_value=["c1"]):
        with patch.object(ks, "_loginctl_session_props", return_value=props):
            assert ks._detect_desktop_session() is None


def test_detect_desktop_session_skips_inactive_graphical_session() -> None:
    props = {
        "Type": "wayland",
        "Active": "no",
        "State": "online",
        "Remote": "no",
        "User": "1000",
    }
    with patch.object(ks, "_loginctl_sessions", return_value=["c1"]):
        with patch.object(ks, "_loginctl_session_props", return_value=props):
            assert ks._detect_desktop_session() is None


def test_loginctl_sessions_empty_when_binary_absent() -> None:
    """No loginctl (no systemd-logind) → no managed desktop → cage path."""
    with patch("shutil.which", return_value=None):
        assert ks._loginctl_sessions() == []


def test_session_env_wayland_sets_wayland_display_and_runtime_dir() -> None:
    session = ks.DesktopSession(
        uid=1000, session_type="wayland", display=None, wayland_display="wayland-1"
    )
    env = ks._session_env(session)
    assert env["XDG_RUNTIME_DIR"] == "/run/user/1000"
    assert env["WAYLAND_DISPLAY"] == "wayland-1"
    assert "DISPLAY" not in env


def test_session_env_x11_sets_display_and_xauthority() -> None:
    session = ks.DesktopSession(uid=1000, session_type="x11", display=":0", wayland_display=None)
    with patch.object(ks, "_xauthority_for", return_value="/home/op/.Xauthority"):
        env = ks._session_env(session)
    assert env["DISPLAY"] == ":0"
    assert env["XAUTHORITY"] == "/home/op/.Xauthority"
    assert env["XDG_RUNTIME_DIR"] == "/run/user/1000"


# ---------------------------------------------------------------------------
# Supervisor env overlay + orphan-sweep gating
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_spawn_merges_env_overlay_over_service_env() -> None:
    """A windowed launch passes a merged env (service env + overlay) so the
    child can reach the running desktop's display server."""
    captured: dict[str, Any] = {}

    async def _fake_exec(*_args: Any, **kwargs: Any) -> _FakeProc:
        captured.update(kwargs)
        return _FakeProc()

    sup = KioskSupervisor(["chromium", "http://x"], env={"DISPLAY": ":0"})
    with patch("asyncio.create_subprocess_exec", _fake_exec):
        await sup._spawn()
    env = captured["env"]
    assert env is not None
    assert env["DISPLAY"] == ":0"
    # Inherits the service env (e.g. PATH) rather than replacing it.
    assert "PATH" in env


@pytest.mark.asyncio
async def test_spawn_inherits_env_when_no_overlay() -> None:
    """The cage path passes env=None so the child inherits the service env."""
    captured: dict[str, Any] = {}

    async def _fake_exec(*_args: Any, **kwargs: Any) -> _FakeProc:
        captured.update(kwargs)
        return _FakeProc()

    sup = KioskSupervisor(["cage", "--"])
    with patch("asyncio.create_subprocess_exec", _fake_exec):
        await sup._spawn()
    assert captured["env"] is None


@pytest.mark.asyncio
async def test_graceful_kill_skips_sweep_when_disabled() -> None:
    """Inside a running desktop (sweep_orphans=False) the broad chromium pkill
    must not run, so the operator's own browser windows survive."""
    proc = _FakeProc()
    sup = KioskSupervisor(["chromium", "http://x"], sweep_orphans=False)
    sweep_calls = 0

    async def _tracking_sweep() -> None:
        nonlocal sweep_calls
        sweep_calls += 1

    with patch.object(sup, "_sweep_orphans", new=_tracking_sweep):
        await sup._graceful_kill(proc)
    assert sweep_calls == 0


@pytest.mark.asyncio
async def test_graceful_kill_runs_sweep_when_enabled() -> None:
    """The cage path sweeps orphaned cage / chromium processes on stop."""
    proc = _FakeProc()
    sup = KioskSupervisor(["cage", "--"])  # sweep_orphans defaults to True
    sweep_calls = 0

    async def _tracking_sweep() -> None:
        nonlocal sweep_calls
        sweep_calls += 1

    with patch.object(sup, "_sweep_orphans", new=_tracking_sweep):
        await sup._graceful_kill(proc)
    assert sweep_calls == 1


# ---------------------------------------------------------------------------
# KioskSupervisor lifecycle
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_supervisor_returns_3_when_binary_missing() -> None:
    """A missing ``cage`` binary yields rc=3 with no restart loop."""
    sup = KioskSupervisor(["cage", "--", "chromium-browser", "http://x"])
    with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError("cage")):
        rc = await sup.run()
    assert rc == 3


@pytest.mark.asyncio
async def test_supervisor_returns_4_on_unexpected_spawn_failure() -> None:
    sup = KioskSupervisor(["cage", "--"])
    with patch("asyncio.create_subprocess_exec", side_effect=RuntimeError("oops")):
        rc = await sup.run()
    assert rc == 4


@pytest.mark.asyncio
async def test_supervisor_stop_event_triggers_graceful_terminate() -> None:
    """A stop request mid-run sends SIGTERM and waits for clean exit."""
    proc = _FakeProc(wait_delay=10.0, sigterm_honored=True)
    sup = KioskSupervisor(["cage", "--"])

    async def _fake_exec(*_args: Any, **_kwargs: Any) -> _FakeProc:
        # Request stop after the spawn returns so the supervisor enters
        # its select loop and sees the stop fire mid-flight.
        sup.request_stop()
        return proc

    with patch("asyncio.create_subprocess_exec", _fake_exec):
        # Patch the orphan sweep so we don't shell out to pkill.
        with patch.object(sup, "_sweep_orphans", new=_async_noop):
            rc = await sup.run()

    assert rc == 0
    assert proc._terminate_called is True
    assert proc._kill_called is False


@pytest.mark.asyncio
async def test_supervisor_escalates_to_sigkill_when_terminate_ignored(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """If the child ignores SIGTERM the supervisor must escalate to SIGKILL."""
    proc = _FakeProc(wait_delay=10.0, sigterm_honored=False)
    sup = KioskSupervisor(["cage", "--"])

    async def _fake_exec(*_args: Any, **_kwargs: Any) -> _FakeProc:
        sup.request_stop()
        return proc

    # Cut the grace window so the test stays fast.
    monkeypatch.setattr(ks, "_SHUTDOWN_GRACE_SECONDS", 0.05)

    with patch("asyncio.create_subprocess_exec", _fake_exec):
        with patch.object(sup, "_sweep_orphans", new=_async_noop):
            rc = await sup.run()

    assert rc == 0
    assert proc._terminate_called is True
    assert proc._kill_called is True


@pytest.mark.asyncio
async def test_supervisor_crash_loop_guard_stops_after_5_in_60s(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """5 crashes in the rolling 60s window must stop the restart loop."""
    spawn_count = 0

    async def _fake_exec(*_args: Any, **_kwargs: Any) -> _FakeProc:
        nonlocal spawn_count
        spawn_count += 1
        # Each child exits with rc=2 immediately.
        return _FakeProc(wait_result=2, wait_delay=0.0)

    # Zero out the backoff so the loop runs at test speed.
    monkeypatch.setattr(ks, "_BACKOFF_START_SECONDS", 0.0)
    monkeypatch.setattr(ks, "_BACKOFF_MAX_SECONDS", 0.0)

    sup = KioskSupervisor(["cage", "--"])
    with patch("asyncio.create_subprocess_exec", _fake_exec):
        rc = await sup.run()

    assert spawn_count == 5
    # The loop returns the last observed rc (or 5 if it was negative).
    assert rc in (2, 5)


def test_record_crash_under_limit() -> None:
    """First four crashes stay under the limit, the fifth flips it."""
    sup = KioskSupervisor(["cage"])
    assert sup._record_crash_and_check() is True
    assert sup._record_crash_and_check() is True
    assert sup._record_crash_and_check() is True
    assert sup._record_crash_and_check() is True
    # Fifth crash pushes len() to 5, which is NOT under the limit (==).
    assert sup._record_crash_and_check() is False


def test_tail_bytes_returns_trailing_window() -> None:
    """The stderr tail surfaces only the last N bytes, decoded loosely."""
    payload = b"A" * 100 + b"\xffMARKER"
    out = KioskSupervisor._tail_bytes(payload, limit=10)
    assert "MARKER" in out


def test_tail_bytes_empty_input() -> None:
    assert KioskSupervisor._tail_bytes(b"") == ""


# ---------------------------------------------------------------------------
# Helpers used by tests
# ---------------------------------------------------------------------------


async def _async_noop() -> None:
    return None
