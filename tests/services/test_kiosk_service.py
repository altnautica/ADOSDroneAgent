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
import os
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


def _drm_dirs(tmp_path: Any, connectors: dict[str, str], cards: list[str]) -> tuple[Any, Any]:
    """Build fake /sys/class/drm (card*-*/status) + /dev/dri (card*) trees."""
    sysfs = tmp_path / "sys_drm"
    dev = tmp_path / "dev_dri"
    sysfs.mkdir()
    dev.mkdir()
    for name, status in connectors.items():
        c = sysfs / name
        c.mkdir()
        (c / "status").write_text(status + "\n")
    for card in cards:
        (dev / card).write_text("")
    return sysfs, dev


def test_hdmi_present_true_when_a_connector_is_connected(tmp_path: Any) -> None:
    # On a Pi the display is card1, not card0 — a connected connector on ANY
    # card counts.
    sysfs, dev = _drm_dirs(
        tmp_path, {"card1-HDMI-A-1": "connected", "card1-HDMI-A-2": "disconnected"}, ["card0", "card1"]
    )
    with patch.object(ks, "_DRM_SYSFS", sysfs), patch.object(ks, "_DRM_DIR", dev):
        assert _hdmi_present() is True


def test_hdmi_present_true_fallback_when_card_node_exists(tmp_path: Any) -> None:
    # No connector status readable, but a DRM card node exists -> the subsystem
    # is up, so proceed (fallback).
    sysfs, dev = _drm_dirs(tmp_path, {}, ["card0"])
    with patch.object(ks, "_DRM_SYSFS", sysfs), patch.object(ks, "_DRM_DIR", dev):
        assert _hdmi_present() is True


def test_hdmi_present_false_when_no_drm(tmp_path: Any) -> None:
    sysfs, dev = _drm_dirs(tmp_path, {"card1-HDMI-A-1": "disconnected"}, [])
    with patch.object(ks, "_DRM_SYSFS", sysfs), patch.object(ks, "_DRM_DIR", dev):
        assert _hdmi_present() is False


@pytest.mark.asyncio
async def test_wait_for_display_returns_immediately_when_present() -> None:
    with patch.object(ks, "_hdmi_present", return_value=True):
        assert await ks._wait_for_display() is True


@pytest.mark.asyncio
async def test_wait_for_display_times_out_headless(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(ks, "_DISPLAY_WAIT_SECONDS", 0.05)
    monkeypatch.setattr(ks, "_DISPLAY_POLL_SECONDS", 0.01)
    with patch.object(ks, "_hdmi_present", return_value=False):
        assert await ks._wait_for_display() is False


@pytest.mark.asyncio
async def test_wait_for_display_appears_after_poll(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(ks, "_DISPLAY_WAIT_SECONDS", 1.0)
    monkeypatch.setattr(ks, "_DISPLAY_POLL_SECONDS", 0.01)
    calls = {"n": 0}

    def _present() -> bool:
        calls["n"] += 1
        return calls["n"] >= 3  # appears on the 3rd check

    with patch.object(ks, "_hdmi_present", side_effect=_present):
        assert await ks._wait_for_display() is True


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
        argv = _build_chromium_argv("http://target/hud", ks._RENDERER_SOFTWARE)
    assert argv[0] == "cage"
    assert argv[1] == "--"
    assert argv[2] == "/usr/bin/chromium"
    assert "--kiosk" in argv
    assert "--ozone-platform=wayland" in argv
    # cage runs Chromium as root, so it needs --no-sandbox.
    assert "--no-sandbox" in argv
    assert argv[-1] == "http://target/hud"


def test_build_chromium_argv_software_disables_gpu() -> None:
    """Software renderer -> --disable-gpu, and NO EGL/GPU-raster flags (so
    Chromium never opens a GPU EGL that the box cannot drive)."""
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        argv = _build_chromium_argv("http://x", ks._RENDERER_SOFTWARE)
    assert "--disable-gpu" in argv
    assert "--use-gl=egl" not in argv
    assert "--enable-gpu-rasterization" not in argv


def test_build_chromium_argv_gpu_uses_egl() -> None:
    """GPU renderer -> EGL + GPU rasterization, and NOT --disable-gpu."""
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        argv = _build_chromium_argv("http://x", ks._RENDERER_GPU)
    assert "--use-gl=egl" in argv
    assert "--enable-gpu-rasterization" in argv
    assert "--disable-gpu" not in argv


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
        argv = ks._build_windowed_chromium_argv(
            "http://target/cockpit", "wayland", ks._RENDERER_SOFTWARE
        )
    assert "cage" not in argv
    assert argv[0] == "/usr/bin/chromium"
    assert "--kiosk" in argv
    assert "--ozone-platform=wayland" in argv
    assert argv[-1] == "http://target/cockpit"


def test_windowed_argv_x11_uses_x11_platform() -> None:
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        argv = ks._build_windowed_chromium_argv(
            "http://target/cockpit", "x11", ks._RENDERER_SOFTWARE
        )
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
# Renderer selection: software default, GPU opt-in via the install marker,
# scoped libmali, and the GPU->software self-heal.
# ---------------------------------------------------------------------------


def test_normalise_renderer_aliases() -> None:
    assert ks._normalise_renderer("gpu") == ks._RENDERER_GPU
    assert ks._normalise_renderer("GLES2") == ks._RENDERER_GPU
    assert ks._normalise_renderer("egl") == ks._RENDERER_GPU
    assert ks._normalise_renderer("software") == ks._RENDERER_SOFTWARE
    assert ks._normalise_renderer("pixman") == ks._RENDERER_SOFTWARE
    assert ks._normalise_renderer("cpu") == ks._RENDERER_SOFTWARE
    assert ks._normalise_renderer("nonsense") is None


def test_read_render_marker_parses_renderer_and_lib_dir(tmp_path: Any) -> None:
    marker = tmp_path / "kiosk-render.conf"
    marker.write_text("# provisioned by installer\nrenderer: gpu\nlib_dir: /opt/ados/gpu/mali\n")
    with patch.object(ks, "_RENDER_MARKER_PATH", marker):
        renderer, lib_dir = ks._read_render_marker()
    assert renderer == ks._RENDERER_GPU
    assert lib_dir == "/opt/ados/gpu/mali"


def test_read_render_marker_missing_file_returns_nones(tmp_path: Any) -> None:
    with patch.object(ks, "_RENDER_MARKER_PATH", tmp_path / "absent.conf"):
        assert ks._read_render_marker() == (None, None)


def test_resolve_render_plan_defaults_to_software(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("ADOS_KIOSK_RENDERER", raising=False)
    with patch.object(ks, "_read_render_marker", return_value=(None, None)):
        assert ks._resolve_render_plan() == (ks._RENDERER_SOFTWARE, None)


def test_resolve_render_plan_env_override_software_wins(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("ADOS_KIOSK_RENDERER", "software")
    with patch.object(ks, "_read_render_marker", return_value=(ks._RENDERER_GPU, "/x")):
        assert ks._resolve_render_plan() == (ks._RENDERER_SOFTWARE, None)


def test_resolve_render_plan_marker_gpu_requires_existing_lib_dir(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Any
) -> None:
    monkeypatch.delenv("ADOS_KIOSK_RENDERER", raising=False)
    lib_dir = tmp_path / "mali"
    lib_dir.mkdir()
    with patch.object(ks, "_read_render_marker", return_value=(ks._RENDERER_GPU, str(lib_dir))):
        assert ks._resolve_render_plan() == (ks._RENDERER_GPU, str(lib_dir))


def test_resolve_render_plan_marker_gpu_stale_lib_dir_falls_back(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A marker that says gpu but whose scoped lib dir is gone -> software."""
    monkeypatch.delenv("ADOS_KIOSK_RENDERER", raising=False)
    with patch.object(
        ks, "_read_render_marker", return_value=(ks._RENDERER_GPU, "/opt/ados/gpu/gone")
    ):
        assert ks._resolve_render_plan() == (ks._RENDERER_SOFTWARE, None)


def test_cage_env_software_uses_pixman_no_ld_path() -> None:
    env = ks._cage_env(ks._RENDERER_SOFTWARE, None)
    assert env["WLR_RENDERER"] == "pixman"
    assert env["WLR_DRM_DEVICES"] == ks._DRM_DEVICE
    assert env["WLR_NO_HARDWARE_CURSORS"] == "1"
    assert "LD_LIBRARY_PATH" not in env


def test_cage_env_gpu_scopes_libmali_and_pins_gles2(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("LD_LIBRARY_PATH", raising=False)
    env = ks._cage_env(ks._RENDERER_GPU, "/opt/ados/gpu/mali")
    assert env["WLR_RENDERER"] == "gles2"
    assert env["LD_LIBRARY_PATH"] == "/opt/ados/gpu/mali"
    # No EGL_PLATFORM: cage picks GBM and Chromium picks Wayland, each explicitly.
    assert "EGL_PLATFORM" not in env
    # A pixman-only crutch does not apply on the GPU path.
    assert "WLR_NO_HARDWARE_CURSORS" not in env


def test_cage_env_gpu_prepends_to_existing_ld_path(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("LD_LIBRARY_PATH", "/usr/lib/existing")
    env = ks._cage_env(ks._RENDERER_GPU, "/opt/ados/gpu/mali")
    assert env["LD_LIBRARY_PATH"] == "/opt/ados/gpu/mali:/usr/lib/existing"


def test_cage_env_gpu_without_lib_dir_no_ld_path() -> None:
    """GPU with a system-wide (unspecified) libmali gets no scoped LD path."""
    env = ks._cage_env(ks._RENDERER_GPU, None)
    assert env["WLR_RENDERER"] == "gles2"
    assert "LD_LIBRARY_PATH" not in env


def test_chromium_render_flags() -> None:
    assert ks._chromium_render_flags(ks._RENDERER_SOFTWARE) == ["--disable-gpu"]
    assert ks._chromium_render_flags(ks._RENDERER_GPU) == [
        "--use-gl=egl",
        "--enable-gpu-rasterization",
    ]


def test_looks_gpu_failure_matches_markers() -> None:
    assert ks._looks_gpu_failure("EGL_NOT_INITIALIZED: DRI2 failed to create screen") is True
    assert ks._looks_gpu_failure("failed to create renderer") is True
    assert ks._looks_gpu_failure("clean shutdown, no errors") is False


# ---------------------------------------------------------------------------
# env_unset strips DISPLAY on the cage path; _make_supervisor path selection
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_spawn_env_unset_strips_display() -> None:
    """The cage path passes env_unset={DISPLAY,...} so cage never sees a stale
    DISPLAY (the historical X11-backend crash)."""
    captured: dict[str, Any] = {}

    async def _fake_exec(*_args: Any, **kwargs: Any) -> _FakeProc:
        captured.update(kwargs)
        return _FakeProc()

    sup = KioskSupervisor(
        ["cage", "--"],
        env={"WLR_RENDERER": "pixman"},
        env_unset=frozenset({"DISPLAY", "WAYLAND_DISPLAY"}),
    )
    with patch.dict("os.environ", {"DISPLAY": ":0", "PATH": "/usr/bin"}, clear=False):
        with patch("asyncio.create_subprocess_exec", _fake_exec):
            await sup._spawn()
    env = captured["env"]
    assert env is not None
    assert "DISPLAY" not in env
    assert env["WLR_RENDERER"] == "pixman"
    assert "PATH" in env  # still inherits the service env otherwise


def test_make_supervisor_cage_gpu_strips_display_and_scopes_libmali() -> None:
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        sup = ks._make_supervisor("http://x", None, ks._RENDERER_GPU, "/opt/ados/gpu/mali")
    assert sup._argv[0] == "cage"
    assert "--use-gl=egl" in sup._argv
    assert sup._env is not None
    assert sup._env["WLR_RENDERER"] == "gles2"
    assert sup._env["LD_LIBRARY_PATH"].startswith("/opt/ados/gpu/mali")
    assert sup._env_unset is not None and "DISPLAY" in sup._env_unset
    assert sup._sweep_orphans_enabled is True


def test_make_supervisor_windowed_forces_software_even_when_gpu_requested() -> None:
    """A live desktop owns its own GL; our scoped GPU userspace does not touch
    it, so the windowed browser renders in software regardless of the marker."""
    session = ks.DesktopSession(
        uid=1000, session_type="wayland", display=None, wayland_display="wayland-0"
    )
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        with patch.object(ks, "_session_env", return_value={"XDG_RUNTIME_DIR": "/run/user/1000"}):
            sup = ks._make_supervisor(
                "http://x", session, ks._RENDERER_GPU, "/opt/ados/gpu/mali"
            )
    assert "--disable-gpu" in sup._argv
    assert "--use-gl=egl" not in sup._argv
    assert sup._sweep_orphans_enabled is False


def test_make_supervisor_windowed_runs_as_the_session_user() -> None:
    """The windowed browser drops to the desktop user (Chromium refuses root);
    the argv must NOT carry --no-sandbox (the sandbox is kept as a normal user)."""
    session = ks.DesktopSession(
        uid=1000, session_type="wayland", display=None, wayland_display="wayland-0"
    )
    with patch.object(ks, "_resolve_browser_binary", return_value="/usr/bin/chromium"):
        with patch.object(ks, "_session_env", return_value={"XDG_RUNTIME_DIR": "/run/user/1000"}):
            sup = ks._make_supervisor("http://x", session, ks._RENDERER_SOFTWARE, None)
    assert sup._run_as_uid == 1000
    assert "--no-sandbox" not in sup._argv  # runs as a user, keeps its sandbox


def test_session_env_carries_home_for_the_session_user() -> None:
    """The windowed launch runs as the user, so it needs a writable HOME (not the
    service's /root). Use the current process uid, which always resolves."""
    uid = os.getuid()
    session = ks.DesktopSession(
        uid=uid, session_type="wayland", display=None, wayland_display="wayland-0"
    )
    env = ks._session_env(session)
    assert env.get("HOME")  # present + non-empty
    assert env.get("USER")


@pytest.mark.asyncio
async def test_spawn_drops_to_uid_when_run_as_uid_set() -> None:
    """A supervisor with run_as_uid passes user=/group= to the spawn so the child
    is dropped to the desktop user before exec."""
    captured: dict[str, Any] = {}

    async def _fake_exec(*_args: Any, **kwargs: Any) -> _FakeProc:
        captured.update(kwargs)
        return _FakeProc()

    uid = os.getuid()
    sup = KioskSupervisor(["chromium", "http://x"], run_as_uid=uid)
    with patch("asyncio.create_subprocess_exec", _fake_exec):
        await sup._spawn()
    assert captured.get("user") == uid
    assert "group" in captured  # primary gid resolved


@pytest.mark.asyncio
async def test_spawn_no_uid_drop_by_default() -> None:
    """The cage path (no run_as_uid) passes no user/group — it runs as the
    service user (root)."""
    captured: dict[str, Any] = {}

    async def _fake_exec(*_args: Any, **kwargs: Any) -> _FakeProc:
        captured.update(kwargs)
        return _FakeProc()

    sup = KioskSupervisor(["cage", "--"])
    with patch("asyncio.create_subprocess_exec", _fake_exec):
        await sup._spawn()
    assert "user" not in captured
    assert "group" not in captured


# ---------------------------------------------------------------------------
# Display-manager-aware session wait (the KDE boot race)
# ---------------------------------------------------------------------------


def test_display_manager_active_true_when_a_dm_is_active() -> None:
    def _fake_run(argv: list[str], **_kw: Any) -> Any:
        # `systemctl is-active sddm.service` -> active
        active = argv[-1] == "sddm.service"
        return SimpleNamespace(stdout="active\n" if active else "inactive\n", returncode=0)

    with patch("shutil.which", return_value="/usr/bin/systemctl"):
        with patch("subprocess.run", side_effect=_fake_run):
            assert ks._display_manager_active() is True


def test_display_manager_active_false_when_none_active() -> None:
    def _fake_run(_argv: list[str], **_kw: Any) -> Any:
        return SimpleNamespace(stdout="inactive\n", returncode=3)

    with patch("shutil.which", return_value="/usr/bin/systemctl"):
        with patch("subprocess.run", side_effect=_fake_run):
            assert ks._display_manager_active() is False


@pytest.mark.asyncio
async def test_resolve_desktop_session_returns_immediately_when_present() -> None:
    session = ks.DesktopSession(uid=1, session_type="x11", display=":0", wayland_display=None)
    with patch.object(ks, "_detect_desktop_session", return_value=session):
        with patch.object(ks, "_session_socket_ready", return_value=True):
            got = await ks._resolve_desktop_session()
    assert got is session


@pytest.mark.asyncio
async def test_resolve_desktop_session_waits_for_socket_ready(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A session is 'active' but its display-server socket is not up yet — the
    kiosk waits (avoiding Chromium's 'Failed to connect to Wayland display')."""
    monkeypatch.setattr(ks, "_SESSION_WAIT_SECONDS", 1.0)
    monkeypatch.setattr(ks, "_SESSION_POLL_SECONDS", 0.01)
    session = ks.DesktopSession(uid=1, session_type="wayland", display=None, wayland_display="wayland-0")
    ready = {"n": 0}

    def _sock_ready(_s: ks.DesktopSession) -> bool:
        ready["n"] += 1
        return ready["n"] >= 3  # socket comes up on the 3rd check

    with patch.object(ks, "_detect_desktop_session", return_value=session):
        with patch.object(ks, "_session_socket_ready", side_effect=_sock_ready):
            with patch.object(ks, "_display_manager_active", return_value=True):
                got = await ks._resolve_desktop_session()
    assert got is session


def test_session_socket_ready_wayland(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The wayland socket must actually exist at /run/user/<uid>/<display>."""
    session = ks.DesktopSession(uid=4242, session_type="wayland", display=None, wayland_display="wayland-0")
    runtime = tmp_path / "run_user_4242"
    runtime.mkdir()
    real_path = ks.Path

    def fake_path(p: object) -> object:
        return runtime if str(p) == "/run/user/4242" else real_path(p)

    monkeypatch.setattr(ks, "Path", fake_path)
    # Socket file absent -> not ready.
    assert ks._session_socket_ready(session) is False
    # Socket file present -> ready.
    (runtime / "wayland-0").write_text("")
    assert ks._session_socket_ready(session) is True


def test_session_socket_ready_x11(tmp_path, monkeypatch: pytest.MonkeyPatch) -> None:
    """The X11 socket must exist at /tmp/.X11-unix/X<n>."""
    session = ks.DesktopSession(uid=1, session_type="x11", display=":0", wayland_display=None)
    x11 = tmp_path / "X11-unix"
    x11.mkdir()
    real_path = ks.Path

    def fake_path(p: object) -> object:
        return x11 / "X0" if str(p) == "/tmp/.X11-unix/X0" else real_path(p)

    monkeypatch.setattr(ks, "Path", fake_path)
    assert ks._session_socket_ready(session) is False
    (x11 / "X0").write_text("")
    assert ks._session_socket_ready(session) is True


def test_detect_ready_session_gates_on_socket() -> None:
    """A detected session is only returned once its display socket is up."""
    session = ks.DesktopSession(uid=1, session_type="wayland", display=None, wayland_display="wayland-0")
    with patch.object(ks, "_detect_desktop_session", return_value=session):
        with patch.object(ks, "_session_socket_ready", return_value=False):
            assert ks._detect_ready_session() is None
        with patch.object(ks, "_session_socket_ready", return_value=True):
            assert ks._detect_ready_session() is session
    with patch.object(ks, "_detect_desktop_session", return_value=None):
        assert ks._detect_ready_session() is None


@pytest.mark.asyncio
async def test_resolve_desktop_session_none_when_no_dm() -> None:
    """No session AND no display manager -> genuinely headless -> cage."""
    with patch.object(ks, "_detect_desktop_session", return_value=None):
        with patch.object(ks, "_display_manager_active", return_value=False):
            assert await ks._resolve_desktop_session() is None


@pytest.mark.asyncio
async def test_resolve_desktop_session_waits_then_returns_when_dm_active(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A display manager is active but the session appears only after a poll —
    the kiosk waits for it instead of racing to cage (the KDE boot race)."""
    monkeypatch.setattr(ks, "_SESSION_WAIT_SECONDS", 1.0)
    monkeypatch.setattr(ks, "_SESSION_POLL_SECONDS", 0.01)
    session = ks.DesktopSession(uid=1, session_type="wayland", display=None, wayland_display="wayland-0")
    calls = {"n": 0}

    def _detect() -> ks.DesktopSession | None:
        calls["n"] += 1
        return session if calls["n"] >= 3 else None

    with patch.object(ks, "_detect_desktop_session", side_effect=_detect):
        with patch.object(ks, "_session_socket_ready", return_value=True):
            with patch.object(ks, "_display_manager_active", return_value=True):
                got = await ks._resolve_desktop_session()
    assert got is session


@pytest.mark.asyncio
async def test_resolve_desktop_session_times_out_to_cage(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """DM active but no session ever becomes active -> falls back to cage."""
    monkeypatch.setattr(ks, "_SESSION_WAIT_SECONDS", 0.05)
    monkeypatch.setattr(ks, "_SESSION_POLL_SECONDS", 0.01)
    with patch.object(ks, "_detect_desktop_session", return_value=None):
        with patch.object(ks, "_display_manager_active", return_value=True):
            assert await ks._resolve_desktop_session() is None


# ---------------------------------------------------------------------------
# crash_looped flag + the GPU->software downgrade in _amain
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_crash_loop_sets_crash_looped_flag(monkeypatch: pytest.MonkeyPatch) -> None:
    async def _fake_exec(*_args: Any, **_kwargs: Any) -> _FakeProc:
        return _FakeProc(wait_result=2, wait_delay=0.0)

    monkeypatch.setattr(ks, "_BACKOFF_START_SECONDS", 0.0)
    monkeypatch.setattr(ks, "_BACKOFF_MAX_SECONDS", 0.0)
    sup = KioskSupervisor(["cage", "--"])
    with patch("asyncio.create_subprocess_exec", _fake_exec):
        await sup.run()
    assert sup.crash_looped is True


class _FakeSupervisor:
    """A supervisor stand-in for the _amain downgrade test."""

    def __init__(self, crash: bool) -> None:
        self._crash = crash
        self.crash_looped = False
        self.last_stderr_tail = "EGL_NOT_INITIALIZED" if crash else ""

    def request_stop(self) -> None:
        pass

    async def run(self) -> int:
        self.crash_looped = self._crash
        return 5 if self._crash else 0


@pytest.mark.asyncio
async def test_amain_downgrades_gpu_to_software_on_cage_crash_loop(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A GPU cage launch that crash-loops is retried once in software so the
    cockpit still ends up rendering."""
    renderers_used: list[str] = []
    fakes = [_FakeSupervisor(crash=True), _FakeSupervisor(crash=False)]

    def _fake_make(_url: str, _session: Any, renderer: str, _lib: Any) -> _FakeSupervisor:
        renderers_used.append(renderer)
        return fakes[len(renderers_used) - 1]

    async def _no_session() -> None:
        return None

    monkeypatch.setattr(
        ks, "load_config", lambda: SimpleNamespace(logging=SimpleNamespace(level="info"))
    )
    monkeypatch.setattr(ks, "configure_logging", lambda *a, **k: None)
    monkeypatch.setattr(ks, "_hdmi_present", lambda: True)
    monkeypatch.setattr(ks, "_resolve_target_url", lambda _c: ("http://x", False))
    monkeypatch.setattr(
        ks, "_resolve_render_plan", lambda: (ks._RENDERER_GPU, "/opt/ados/gpu/mali")
    )
    monkeypatch.setattr(ks, "_resolve_desktop_session", _no_session)
    monkeypatch.setattr(ks, "_make_supervisor", _fake_make)

    rc = await ks._amain()
    assert renderers_used == [ks._RENDERER_GPU, ks._RENDERER_SOFTWARE]
    assert rc == 0


# ---------------------------------------------------------------------------
# Helpers used by tests
# ---------------------------------------------------------------------------


async def _async_noop() -> None:
    return None
