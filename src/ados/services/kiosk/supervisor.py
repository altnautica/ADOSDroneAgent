"""HDMI kiosk: supervision of the cage / Chromium child process."""

from __future__ import annotations

import asyncio
import os
import pwd
import subprocess
import time
from typing import Any

from ados.services.kiosk._log import log
from ados.services.kiosk.desktop_session import _wayland_sockets
from ados.services.kiosk.display_probe import _connected_outputs
from ados.services.kiosk.launcher import _BROWSER_CANDIDATES, _CAGE_RUNTIME_DIR

# How long to keep retrying the output-mode set after a cage launch while the
# compositor brings its Wayland socket up.
_OUTPUT_MODE_WAIT_SECONDS = 30.0
_OUTPUT_MODE_POLL_SECONDS = 1.0

# Substrings in a child's stderr that mark a GPU/EGL/renderer init failure,
# used to decide whether a GPU-mode crash should downgrade to software.
_GPU_FAILURE_MARKERS = (
    "failed to create renderer",
    "failed to load driver",
    "eglinitialize",
    "egl_not_initialized",
    "dri2",
    "gbm",
    "could not match drm and vulkan",
    "no drm fd",
    "wlr_renderer",
)

# Crash-loop guard: this many exits inside the window marks the child as
# crash-looping (the trigger for the GPU->software downgrade).
_CRASH_WINDOW_SECONDS = 60.0
_CRASH_LIMIT = 5
# Fixed retry between child restarts. A recovery loop that backs off leaves
# the panel black for longer the longer the fault lasts; retry at a steady
# short interval instead.
_RETRY_SECONDS = 3.0

# Graceful shutdown allowance for the cage child.
_SHUTDOWN_GRACE_SECONDS = 10.0

_STDERR_TAIL_BYTES = 2048

# How many child stderr lines are forwarded to the journal before the rest are
# suppressed. A browser is chatty; the point is to make a failure visible, not
# to relay every frame's noise onto a flash-backed journal.
_STDERR_LOG_LINE_LIMIT = 40

# The browser is spawned BY the compositor, so it is legitimately absent for a
# moment after launch. Wait this long before concluding it is missing.
_BROWSER_START_GRACE_SECONDS = 25.0
_BROWSER_POLL_SECONDS = 10.0


class KioskSupervisor:
    """Spawn and supervise the cage + Chromium child process."""

    def __init__(
        self,
        argv: list[str],
        *,
        env: dict[str, str] | None = None,
        env_unset: frozenset[str] | set[str] | None = None,
        sweep_orphans: bool = True,
        run_as_uid: int | None = None,
        output_mode: str | None = None,
    ) -> None:
        self._argv = argv
        # When set, the child is dropped to this uid (and its primary gid) before
        # exec — the windowed path runs Chromium as the logged-in desktop user
        # rather than root, because Chromium refuses to run as root without
        # --no-sandbox. None (the cage path) runs as the service user (root).
        self._run_as_uid = run_as_uid
        # An environment overlay merged over the service env (used to attach a
        # windowed launch to a running desktop's display server, or to pin
        # cage's renderer/DRM device). None inherits the service env unchanged.
        self._env = env
        # Keys to REMOVE from the child env after the overlay merge. The cage
        # path strips DISPLAY / WAYLAND_DISPLAY so cage uses its own DRM backend
        # instead of trying (and failing) to nest under an absent X11/Wayland
        # server — the root of the historical "Failed to open xcb connection".
        self._env_unset = env_unset
        # Whether to broad-pkill cage/chromium orphans on stop. True under cage
        # (safe — cage owns the only chromium). False inside a running desktop,
        # where a broad chromium sweep would kill the operator's own browser.
        self._sweep_orphans_enabled = sweep_orphans
        # The ``WIDTHxHEIGHT`` mode to set on the connected outputs once the
        # compositor is up (cage path only). None leaves the mode alone.
        self._output_mode = output_mode
        self._proc: asyncio.subprocess.Process | None = None
        self._stop = asyncio.Event()
        self._crash_times: list[float] = []
        # Set True when the crash-loop guard trips (5 crashes / 60 s). Lets the
        # caller downgrade a crash-looping GPU launch to the software renderer.
        self.crash_looped = False
        # Last child's stderr tail, for the caller's downgrade heuristic.
        self.last_stderr_tail = ""

    def request_stop(self) -> None:
        self._stop.set()

    async def _spawn(self) -> asyncio.subprocess.Process:
        log.info("kiosk_spawning", argv=self._argv, run_as_uid=self._run_as_uid)
        spawn_env: dict[str, str] | None
        if self._env is None and not self._env_unset:
            spawn_env = None
        else:
            spawn_env = {**os.environ, **(self._env or {})}
            for key in self._env_unset or ():
                spawn_env.pop(key, None)
        # Drop to the desktop user for the windowed path (Chromium refuses root).
        # `user`/`group` setgid+setuid before exec (Python 3.9+). We resolve the
        # primary gid ourselves so supplementary groups are dropped too.
        extra: dict[str, Any] = {}
        if self._run_as_uid is not None:
            extra["user"] = self._run_as_uid
            try:
                extra["group"] = pwd.getpwuid(self._run_as_uid).pw_gid
            except KeyError:
                pass
        return await asyncio.create_subprocess_exec(
            *self._argv,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=spawn_env,
            # Own session so a windowed Chromium's whole tree is killable via
            # the child, without a broad pkill that would hit the desktop's
            # other browsers.
            start_new_session=True,
            **extra,
        )

    async def _graceful_kill(self, proc: asyncio.subprocess.Process) -> None:
        if proc.returncode is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                pass
            else:
                try:
                    await asyncio.wait_for(
                        proc.wait(), timeout=_SHUTDOWN_GRACE_SECONDS
                    )
                    log.info("kiosk_child_terminated", rc=proc.returncode)
                except TimeoutError:
                    log.warning("kiosk_child_sigterm_timeout", pid=proc.pid)
                    try:
                        proc.kill()
                        await proc.wait()
                        log.warning("kiosk_child_killed", rc=proc.returncode)
                    except ProcessLookupError:
                        pass

        # cage may leave an orphaned chromium-browser process when it is
        # torn down under load. Sweep both names best-effort so systemd
        # sees a clean exit. Idempotent: pkill returns non-zero when
        # nothing matched, which is fine. Skipped inside a running desktop,
        # where a broad chromium pkill would also kill the operator's own
        # browser windows — there, terminating our own child (a Chromium that
        # shuts its tree down on SIGTERM) is enough.
        if self._sweep_orphans_enabled:
            await self._sweep_orphans()

    async def _sweep_orphans(self) -> None:
        """Best-effort pkill sweep of cage and chromium-browser children."""
        for name, first_sig in (("cage", "-TERM"), ("chromium", "-TERM")):
            await self._run_pkill(first_sig, name)
            await asyncio.sleep(1.0)
            await self._run_pkill("-KILL", name)

    @staticmethod
    async def _run_pkill(sig: str, name: str) -> None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "pkill",
                sig,
                "-f",
                name,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            try:
                await asyncio.wait_for(proc.wait(), timeout=3.0)
            except TimeoutError:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
        except (FileNotFoundError, OSError) as exc:
            log.debug("kiosk_pkill_skipped", sig=sig, name=name, error=str(exc))

    def _record_crash_and_check(self) -> bool:
        """Append now() to crash log, prune outside window. Return True if under limit."""
        now = time.monotonic()
        self._crash_times.append(now)
        self._crash_times = [
            t for t in self._crash_times if (now - t) <= _CRASH_WINDOW_SECONDS
        ]
        return len(self._crash_times) < _CRASH_LIMIT

    @staticmethod
    def _tail_bytes(data: bytes, limit: int = _STDERR_TAIL_BYTES) -> str:
        if not data:
            return ""
        trimmed = data[-limit:]
        try:
            return trimmed.decode("utf-8", errors="replace").strip()
        except Exception:
            return ""

    async def _stream_stderr(self, proc: Any) -> None:
        """Forward the child's stderr to the journal line by line, and keep the
        rolling tail the GPU-downgrade heuristic reads.

        Bounded on purpose: a browser can be extremely chatty, and the point is
        to make a failure visible, not to relay every frame's worth of noise
        onto a flash-backed journal.
        """
        if proc.stderr is None:
            return
        seen = 0
        recent: list[str] = []
        try:
            while True:
                raw = await proc.stderr.readline()
                if not raw:
                    break
                line = raw.decode("utf-8", errors="replace").rstrip()
                if not line:
                    continue
                recent.append(line)
                if len(recent) > 40:
                    recent.pop(0)
                self.last_stderr_tail = "\n".join(recent)[-_STDERR_TAIL_BYTES:]
                if seen < _STDERR_LOG_LINE_LIMIT:
                    log.warning("kiosk_child_stderr", line=line[:400])
                elif seen == _STDERR_LOG_LINE_LIMIT:
                    log.warning(
                        "kiosk_child_stderr_suppressed",
                        msg=f"further child stderr suppressed after {seen} lines",
                    )
                seen += 1
        except asyncio.CancelledError:
            raise
        except Exception:
            return

    async def _watch_browser(self, proc: Any) -> None:
        """Resolve once the BROWSER is gone while the compositor is still up.

        Only meaningful on the cage path, where the supervisor's child is the
        compositor and the browser is its grandchild. On the windowed path the
        child IS the browser, so `proc.wait()` already covers it and this never
        fires.

        The browser is given a grace period to appear: it is spawned by cage, so
        it is legitimately absent for a moment right after launch.
        """
        if not self._sweep_orphans_enabled:
            return  # windowed path: the child is the browser itself
        await asyncio.sleep(_BROWSER_START_GRACE_SECONDS)
        while True:
            if proc.returncode is not None:
                return  # the compositor went first; the normal path handles it
            if not await asyncio.to_thread(_browser_running):
                return
            await asyncio.sleep(_BROWSER_POLL_SECONDS)

    async def _apply_output_mode(self) -> None:
        """Set the configured mode on every connected output inside cage.

        Uses ``wlr-randr`` over cage's Wayland socket (cage implements the
        wlr-output-management protocol it drives). Retried on a fixed interval
        until the compositor's socket answers or the wait expires; an output that
        does not advertise the mode keeps the one cage picked. Inert on the
        windowed path, where the desktop owns its display settings.
        """
        mode = self._output_mode
        if mode is None or not self._sweep_orphans_enabled:
            return
        runtime_dir = (self._env or {}).get("XDG_RUNTIME_DIR", _CAGE_RUNTIME_DIR)
        pending: set[str] = set()
        for name, modes in _connected_outputs().items():
            if mode in modes:
                pending.add(name)
            else:
                log.warning("kiosk_output_mode_unsupported", output=name, mode=mode)
        deadline = time.monotonic() + _OUTPUT_MODE_WAIT_SECONDS
        last_error = "compositor socket never appeared"
        while pending and time.monotonic() < deadline:
            sockets = _wayland_sockets(runtime_dir)
            if sockets:
                env = {
                    **os.environ,
                    **(self._env or {}),
                    "WAYLAND_DISPLAY": sockets[0],
                }
                env.pop("DISPLAY", None)
                for name in sorted(pending):
                    try:
                        proc = await asyncio.create_subprocess_exec(
                            "wlr-randr",
                            "--output",
                            name,
                            "--mode",
                            mode,
                            stdout=asyncio.subprocess.DEVNULL,
                            stderr=asyncio.subprocess.PIPE,
                            env=env,
                        )
                    except OSError as exc:
                        log.warning(
                            "kiosk_output_mode_unavailable",
                            mode=mode,
                            error=str(exc),
                        )
                        return
                    _out, err = await proc.communicate()
                    if proc.returncode == 0:
                        log.info("kiosk_output_mode_applied", output=name, mode=mode)
                        pending.discard(name)
                    else:
                        last_error = err.decode("utf-8", errors="replace").strip()
            if pending:
                await asyncio.sleep(_OUTPUT_MODE_POLL_SECONDS)
        for name in sorted(pending):
            log.warning(
                "kiosk_output_mode_failed", output=name, mode=mode, error=last_error
            )

    async def run(self) -> int:
        """Supervise loop. Returns process exit code or 0 on clean stop."""
        while not self._stop.is_set():
            try:
                self._proc = await self._spawn()
            except FileNotFoundError as exc:
                log.error("kiosk_binary_missing", error=str(exc))
                return 3
            except Exception as exc:
                log.error("kiosk_spawn_failed", error=str(exc))
                return 4

            proc = self._proc
            log.info("kiosk_child_running", pid=proc.pid)

            wait_task = asyncio.create_task(proc.wait(), name="kiosk_child_wait")
            stop_task = asyncio.create_task(self._stop.wait(), name="kiosk_stop_wait")
            # Drain the child's stderr WHILE it runs, not only when it exits.
            #
            # A compositor that stays up with a broken browser inside it never
            # exits, so an exit-time read never happens and the error is never
            # seen. That is exactly how a black panel presented as a healthy
            # unit: the last journal line was `kiosk_child_running`, and the GPU
            # initialization error that caused it was only visible by running
            # the argv by hand.
            drain_task = asyncio.create_task(
                self._stream_stderr(proc), name="kiosk_child_stderr"
            )
            # And watch the BROWSER, not just the compositor we launched. The
            # supervisor's child is `cage`; the browser is its grandchild, so a
            # dead browser under a live cage is invisible to `proc.wait()`.
            browser_task = asyncio.create_task(
                self._watch_browser(proc), name="kiosk_browser_watch"
            )
            # Apply the configured output mode once the compositor is up. cage
            # picks the preferred mode on every launch, so this runs per spawn.
            mode_task = asyncio.create_task(
                self._apply_output_mode(), name="kiosk_output_mode"
            )
            done, pending = await asyncio.wait(
                {wait_task, stop_task, browser_task},
                return_when=asyncio.FIRST_COMPLETED,
            )
            if browser_task in done and wait_task not in done and stop_task not in done:
                # The compositor is still alive but has nothing to show. Treat it
                # as a crash so the existing retry + GPU-downgrade machinery
                # handles it, rather than leaving a black screen reading healthy.
                log.error(
                    "kiosk_browser_vanished",
                    msg="the browser exited while the compositor stayed up; restarting",
                    stderr_tail=self.last_stderr_tail,
                )
                for t in pending:
                    t.cancel()
                drain_task.cancel()
                mode_task.cancel()
                await self._graceful_kill(proc)
                self._record_crash_and_check()
                await asyncio.sleep(_RETRY_SECONDS)
                continue
            drain_task.cancel()
            mode_task.cancel()

            if stop_task in done:
                for t in pending:
                    t.cancel()
                await self._graceful_kill(proc)
                log.info("kiosk_supervisor_stopping")
                return 0

            # Child exited on its own.
            for t in pending:
                t.cancel()

            rc = proc.returncode if proc.returncode is not None else -1
            stderr_data = b""
            try:
                if proc.stderr is not None:
                    stderr_data = await asyncio.wait_for(proc.stderr.read(), timeout=1.0)
            except Exception:
                pass
            stderr_tail = self._tail_bytes(stderr_data)
            self.last_stderr_tail = stderr_tail

            under_limit = self._record_crash_and_check()
            log.warning(
                "kiosk_child_exited",
                rc=rc,
                stderr_tail=stderr_tail,
                crashes_in_window=len(self._crash_times),
            )

            if not under_limit:
                self.crash_looped = True
                log.error(
                    "kiosk_crash_loop_guard",
                    msg="5 crashes in 60s, stopping restart loop",
                    last_rc=rc,
                )
                # Never 0: the unit's RestartPreventExitStatus=0 would treat a
                # child that keeps exiting cleanly as "nothing to do" and leave
                # the panel black until a reboot.
                return rc if rc > 0 else 5

            try:
                await asyncio.wait_for(self._stop.wait(), timeout=_RETRY_SECONDS)
                # If stop fires during the retry wait, exit cleanly.
                return 0
            except TimeoutError:
                pass

        return 0


def _browser_running() -> bool:
    """True when a process whose executable is a known browser binary is live.

    Matched on each process's argv[0] basename, never on the whole command
    line: the compositor is launched as ``cage -- /usr/bin/chromium ...``, so a
    full-command-line match (``pgrep -f chromium``) finds cage itself and
    reports a live browser for as long as the compositor lives.

    Errs toward TRUE on any uncertainty (ps missing or failing): a false "the
    browser is gone" would restart a working kiosk, which is worse than missing
    one failure.
    """
    try:
        res = subprocess.run(  # noqa: S603
            ["ps", "-A", "-o", "args="],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return True
    if res.returncode != 0:
        return True
    for line in res.stdout.splitlines():
        argv0 = line.strip().split(" ", 1)[0]
        if os.path.basename(argv0) in _BROWSER_CANDIDATES:
            return True
    return False


def _looks_gpu_failure(stderr_tail: str) -> bool:
    """True when a child's stderr tail carries a GPU/EGL/renderer-init failure
    marker — a diagnostic hint for the GPU->software downgrade (the downgrade
    itself does not depend on it)."""
    low = stderr_tail.lower()
    return any(marker in low for marker in _GPU_FAILURE_MARKERS)
