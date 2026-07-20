"""HDMI kiosk service: Chromium full-screen, pointed at the agent cockpit.

HDMI + touch + gamepad should deliver a standalone field console with no
laptop required. This service owns the HDMI output. The default target is the
agent-served cockpit at ``http://localhost:8080/cockpit`` (a light SPA served
by the agent's own front, not a Next.js build on the box).

Lifecycle:
1. Probe `/dev/dri/card0`. If absent, the box has no HDMI sink connected
   (or the DRM driver did not bind). Log clearly and exit 0 so systemd
   does not churn restarting. Rule 26: the rest of the ground station
   keeps working even without HDMI.
2. Resolve target URL via config -> env var -> default chain.
3. Launch Chromium full-screen, adaptively:
   - When a graphical desktop session is already running on the box (a
     display manager with KDE / GNOME / etc.), launch Chromium as a
     full-screen kiosk window INSIDE that session. cage is NOT used here:
     it needs to own the DRM master, which the running desktop compositor
     already holds, so cage would fight the desktop and churn.
   - When no desktop is present (the appliance case), launch under `cage`,
     a Wayland single-app compositor that owns the display itself.
   The Chromium binary is resolved at runtime (its name varies by distro).
4. Supervise the child. On exit, backoff-restart. Five crashes in 60
   seconds flips to ERROR and we stop restarting so systemd can apply
   its own service-level retry.
5. On SIGTERM: send SIGTERM to the child, wait 10 s for graceful exit,
   SIGKILL if it is still up. Under cage we also sweep orphaned cage /
   chromium processes; inside a running desktop we do NOT broad-sweep
   chromium (that would kill the operator's own browser windows).

Not in scope:
- Bundling the cockpit here. It is served by the agent front at :8080.
- Sub-30 ms DRM-composited low-latency video (a v2 optimization).
"""

from __future__ import annotations

import asyncio
import os
import pwd
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger

log = get_logger("kiosk.kiosk_service")

_DRM_CARD_PATH = Path("/dev/dri/card0")

_DEFAULT_URL = "http://localhost:8080/cockpit"
_ENV_URL_KEY = "ADOS_KIOSK_URL"
_ENV_MINIMAL_KEY = "ADOS_KIOSK_MINIMAL_LAYER"

# Crash-loop guard.
_CRASH_WINDOW_SECONDS = 60.0
_CRASH_LIMIT = 5
_BACKOFF_START_SECONDS = 3.0
_BACKOFF_MAX_SECONDS = 30.0

# Graceful shutdown allowance for the cage child.
_SHUTDOWN_GRACE_SECONDS = 10.0

# Minimal-layer auto-trigger threshold. Boards under 3 GiB default to the
# reduced render path so Chromium stays within its memory envelope.
_MINIMAL_RAM_THRESHOLD_BYTES = 3 * 1024 * 1024 * 1024

_STDERR_TAIL_BYTES = 2048

# Chromium browser binary candidates, in resolution order. The binary name
# varies by distro: Raspberry Pi OS historically shipped `chromium-browser`;
# Debian, Armbian, and current Raspberry Pi OS ship `chromium`
# (`/usr/bin/chromium`). `-stable` is the flatpak/snap-adjacent name some
# images expose. The installer installs whichever apt package is available; the
# kiosk resolves the binary at runtime so it does not depend on one fixed name.
_BROWSER_CANDIDATES = ("chromium-browser", "chromium", "chromium-browser-stable")


def _hdmi_present() -> bool:
    """True when the DRM card node exists.

    We do not try to detect a connected monitor. `/dev/dri/card0` is the
    kernel side of the KMS driver. If HDMI hardware is missing entirely
    (headless image, DRM driver not loaded) the node is absent and we
    cleanly skip the kiosk.
    """
    return _DRM_CARD_PATH.exists()


def hdmi_present() -> bool:
    """Public alias for ``_hdmi_present``.

    The heartbeat enrichment helper imports this to resolve the
    effective display type on boards where ``ground_station.display.type``
    is left at ``auto``. Keeping a public wrapper instead of removing
    the leading-underscore name preserves the in-module call sites that
    use the private form.
    """
    return _hdmi_present()


def _get_kiosk_config(config: Any) -> tuple[str | None, bool | None]:
    """Return (target_url, minimal_layer) from config, if present.

    Reads ``config.ground_station.kiosk`` (the KioskConfig model). Accessed
    defensively with ``getattr`` so a duck-typed test config or a bare object
    without the section resolves to ``(None, None)`` instead of raising. Either
    field may be None when unset.
    """
    gs = getattr(config, "ground_station", None)
    if gs is None:
        return None, None
    kiosk = getattr(gs, "kiosk", None)
    if kiosk is None:
        return None, None
    url = getattr(kiosk, "target_url", None)
    minimal = getattr(kiosk, "minimal_layer", None)
    if isinstance(url, str) and not url.strip():
        url = None
    return url, minimal


def _low_ram_board() -> bool:
    try:
        import psutil
    except Exception:
        return False
    try:
        total = psutil.virtual_memory().total
    except Exception:
        return False
    return total < _MINIMAL_RAM_THRESHOLD_BYTES


def _resolve_target_url(config: Any) -> tuple[str, bool]:
    """Config -> env -> default. Returns (url_with_query, minimal_flag).

    Query string `?layer=minimal` is appended when either the config
    flag is true or the board has less than 3 GiB RAM. An explicit
    env override `ADOS_KIOSK_MINIMAL_LAYER=0` forces the full layer.
    """
    cfg_url, cfg_minimal = _get_kiosk_config(config)

    url = cfg_url or os.environ.get(_ENV_URL_KEY) or _DEFAULT_URL

    minimal = False
    if cfg_minimal is True:
        minimal = True
    elif _low_ram_board():
        minimal = True

    env_minimal = os.environ.get(_ENV_MINIMAL_KEY)
    if env_minimal is not None:
        minimal = env_minimal.strip() not in ("0", "false", "False", "")

    if minimal:
        sep = "&" if "?" in url else "?"
        url = f"{url}{sep}layer=minimal"

    return url, minimal


def _resolve_browser_binary() -> str:
    """Return the first Chromium browser binary present on PATH.

    The binary name varies by distro (see `_BROWSER_CANDIDATES`). Probe the
    known names in order and return the absolute path of the first that
    resolves. Raise `FileNotFoundError` naming every tried candidate when none
    is present, so the supervisor's `kiosk_binary_missing` path reports exactly
    what was searched instead of a bare `chromium-browser` not-found.
    """
    for name in _BROWSER_CANDIDATES:
        found = shutil.which(name)
        if found:
            return found
    raise FileNotFoundError(
        "no Chromium browser binary found on PATH; tried: "
        + ", ".join(_BROWSER_CANDIDATES)
    )


def _build_chromium_argv(url: str) -> list[str]:
    """Full argv for `cage -- <chromium> ...`.

    The browser binary is resolved at runtime (`_resolve_browser_binary`)
    because its package/binary name varies by distro. cage handles the Wayland
    compositor; we ask Chromium to use Wayland + EGL for hardware acceleration.
    Raises `FileNotFoundError` (propagated to the `kiosk_binary_missing` path)
    when no browser is installed.
    """
    browser = _resolve_browser_binary()
    return [
        "cage",
        "--",
        browser,
        "--kiosk",
        "--noerrdialogs",
        "--disable-infobars",
        "--no-first-run",
        "--ozone-platform=wayland",
        "--use-gl=egl",
        "--enable-gpu-rasterization",
        "--autoplay-policy=no-user-gesture-required",
        url,
    ]


# ---------------------------------------------------------------------------
# Adaptive launch: run inside a live desktop when one is present, else own the
# display via cage.
# ---------------------------------------------------------------------------

# Session types loginctl reports for a graphical session.
_GRAPHICAL_SESSION_TYPES = ("wayland", "x11")


@dataclass(frozen=True)
class DesktopSession:
    """A running graphical login session the kiosk can launch a window into."""

    uid: int
    session_type: str  # "wayland" | "x11"
    display: str | None  # X11 DISPLAY (e.g. ":0"); None for wayland
    wayland_display: str | None  # wayland socket name; None for x11


def _loginctl_sessions() -> list[str]:
    """Return the session ids from ``loginctl``, or [] when loginctl is absent
    or fails (no systemd-logind → treat the box as having no managed desktop,
    so the kiosk owns the display via cage)."""
    loginctl = shutil.which("loginctl")
    if not loginctl:
        return []
    try:
        out = subprocess.run(
            [loginctl, "list-sessions", "--no-legend"],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if out.returncode != 0:
        return []
    ids: list[str] = []
    for line in out.stdout.splitlines():
        parts = line.split()
        if parts:
            ids.append(parts[0])
    return ids


def _loginctl_session_props(session_id: str) -> dict[str, str]:
    """Return the ``key=value`` properties of one session, or {} on failure."""
    loginctl = shutil.which("loginctl")
    if not loginctl:
        return {}
    try:
        out = subprocess.run(
            [
                loginctl,
                "show-session",
                session_id,
                "-p",
                "Type",
                "-p",
                "State",
                "-p",
                "Active",
                "-p",
                "Remote",
                "-p",
                "User",
                "-p",
                "Display",
            ],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    if out.returncode != 0:
        return {}
    props: dict[str, str] = {}
    for line in out.stdout.splitlines():
        key, _, value = line.partition("=")
        props[key.strip()] = value.strip()
    return props


def _wayland_display_for(uid: int) -> str:
    """Best-effort discovery of the wayland socket name in the user's runtime
    dir, defaulting to ``wayland-0`` (the common default) when none is found."""
    runtime_dir = f"/run/user/{uid}"
    try:
        names = sorted(
            n
            for n in os.listdir(runtime_dir)
            if n.startswith("wayland-") and not n.endswith(".lock")
        )
    except OSError:
        names = []
    return names[0] if names else "wayland-0"


def _xauthority_for(uid: int) -> str | None:
    """Locate the user's X authority cookie so an X11 launch can authenticate to
    the running X server. Best-effort across the common locations."""
    candidates: list[str] = []
    try:
        home = pwd.getpwuid(uid).pw_dir
        candidates.append(os.path.join(home, ".Xauthority"))
    except KeyError:
        pass
    candidates.append(f"/run/user/{uid}/.mutter-Xwaylandauth")
    candidates.append(f"/run/user/{uid}/gdm/Xauthority")
    for path in candidates:
        try:
            if os.path.exists(path):
                return path
        except OSError:
            continue
    return None


def _detect_desktop_session() -> DesktopSession | None:
    """Return the active graphical login session, or None when the box has no
    running desktop. A None result means the kiosk should own the display via
    cage; a session means it should launch a window into that desktop instead
    (cage cannot, because the desktop compositor already holds the DRM master)."""
    for session_id in _loginctl_sessions():
        props = _loginctl_session_props(session_id)
        stype = props.get("Type", "")
        if stype not in _GRAPHICAL_SESSION_TYPES:
            continue
        if props.get("Remote", "no") == "yes":
            continue
        if props.get("Active") != "yes" and props.get("State") != "active":
            continue
        try:
            uid = int(props.get("User", ""))
        except ValueError:
            continue
        if stype == "wayland":
            return DesktopSession(
                uid=uid,
                session_type="wayland",
                display=None,
                wayland_display=_wayland_display_for(uid),
            )
        return DesktopSession(
            uid=uid,
            session_type="x11",
            display=props.get("Display") or ":0",
            wayland_display=None,
        )
    return None


def _session_env(session: DesktopSession) -> dict[str, str]:
    """The environment overlay that lets a process launched by this service
    connect to the running desktop's display server."""
    env: dict[str, str] = {"XDG_RUNTIME_DIR": f"/run/user/{session.uid}"}
    if session.session_type == "wayland":
        env["WAYLAND_DISPLAY"] = session.wayland_display or "wayland-0"
    else:
        env["DISPLAY"] = session.display or ":0"
        xauth = _xauthority_for(session.uid)
        if xauth:
            env["XAUTHORITY"] = xauth
    return env


def _build_windowed_chromium_argv(url: str, session_type: str) -> list[str]:
    """Full argv for a full-screen Chromium kiosk WITHOUT cage, to run inside an
    already-running desktop session. The Ozone platform matches the session so
    Chromium attaches to the live compositor / X server rather than trying to
    own the display. Raises ``FileNotFoundError`` (propagated to the
    ``kiosk_binary_missing`` path) when no browser is installed."""
    browser = _resolve_browser_binary()
    platform = "wayland" if session_type == "wayland" else "x11"
    return [
        browser,
        "--kiosk",
        "--start-fullscreen",
        "--noerrdialogs",
        "--disable-infobars",
        "--no-first-run",
        f"--ozone-platform={platform}",
        "--use-gl=egl",
        "--enable-gpu-rasterization",
        "--autoplay-policy=no-user-gesture-required",
        url,
    ]


class KioskSupervisor:
    """Spawn and supervise the cage + Chromium child process."""

    def __init__(
        self,
        argv: list[str],
        *,
        env: dict[str, str] | None = None,
        sweep_orphans: bool = True,
    ) -> None:
        self._argv = argv
        # An environment overlay merged over the service env (used to attach a
        # windowed launch to a running desktop's display server). None inherits
        # the service env unchanged (the cage path).
        self._env = env
        # Whether to broad-pkill cage/chromium orphans on stop. True under cage
        # (safe — cage owns the only chromium). False inside a running desktop,
        # where a broad chromium sweep would kill the operator's own browser.
        self._sweep_orphans_enabled = sweep_orphans
        self._proc: asyncio.subprocess.Process | None = None
        self._stop = asyncio.Event()
        self._crash_times: list[float] = []

    def request_stop(self) -> None:
        self._stop.set()

    async def _spawn(self) -> asyncio.subprocess.Process:
        log.info("kiosk_spawning", argv=self._argv)
        spawn_env = None if self._env is None else {**os.environ, **self._env}
        return await asyncio.create_subprocess_exec(
            *self._argv,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=spawn_env,
            # Own session so a windowed Chromium's whole tree is killable via
            # the child, without a broad pkill that would hit the desktop's
            # other browsers.
            start_new_session=True,
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

    async def run(self) -> int:
        """Supervise loop. Returns process exit code or 0 on clean stop."""
        backoff = _BACKOFF_START_SECONDS
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
            backoff = _BACKOFF_START_SECONDS

            wait_task = asyncio.create_task(proc.wait(), name="kiosk_child_wait")
            stop_task = asyncio.create_task(self._stop.wait(), name="kiosk_stop_wait")
            done, pending = await asyncio.wait(
                {wait_task, stop_task}, return_when=asyncio.FIRST_COMPLETED
            )

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

            under_limit = self._record_crash_and_check()
            log.warning(
                "kiosk_child_exited",
                rc=rc,
                stderr_tail=stderr_tail,
                crashes_in_window=len(self._crash_times),
            )

            if not under_limit:
                log.error(
                    "kiosk_crash_loop_guard",
                    msg="5 crashes in 60s, stopping restart loop",
                    last_rc=rc,
                )
                return rc if rc >= 0 else 5

            # Exponential backoff, capped.
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=backoff)
                # If stop fires during backoff, exit cleanly.
                return 0
            except TimeoutError:
                pass
            backoff = min(backoff * 2, _BACKOFF_MAX_SECONDS)

        return 0


async def _amain() -> int:
    config = load_config()
    configure_logging(config.logging.level)
    slog = structlog.get_logger()
    slog.info("kiosk_service_starting")

    if not _hdmi_present():
        slog.info(
            "kiosk_hdmi_absent",
            path=str(_DRM_CARD_PATH),
            msg="no DRM card node, HDMI kiosk skipped cleanly",
        )
        return 0

    url, minimal = _resolve_target_url(config)
    slog.info("kiosk_target_resolved", url=url, minimal_layer=minimal)

    # Adaptive launch: run a full-screen window inside a live desktop when one
    # is present (cage cannot — the desktop already owns the DRM master), else
    # own the display via cage.
    session = _detect_desktop_session()
    try:
        if session is not None:
            slog.info(
                "kiosk_desktop_session_detected",
                session_type=session.session_type,
                uid=session.uid,
            )
            argv = _build_windowed_chromium_argv(url, session.session_type)
            env = _session_env(session)
            supervisor = KioskSupervisor(argv, env=env, sweep_orphans=False)
        else:
            slog.info("kiosk_no_desktop_session", msg="owning the display via cage")
            argv = _build_chromium_argv(url)
            supervisor = KioskSupervisor(argv)
    except FileNotFoundError as exc:
        # No Chromium browser installed. Report which names were searched and
        # exit non-zero (the same rc the supervisor uses when a spawn hits a
        # missing binary) so the failure is visible without churning.
        slog.error("kiosk_binary_missing", error=str(exc))
        return 3

    loop = asyncio.get_event_loop()

    def _on_signal(*_args: Any) -> None:
        slog.info("kiosk_service_signal_stop")
        supervisor.request_stop()

    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, _on_signal)
        except NotImplementedError:
            signal.signal(sig, _on_signal)

    rc = await supervisor.run()
    slog.info("kiosk_service_stopped", rc=rc)
    return rc


def main() -> None:
    try:
        rc = asyncio.run(_amain())
    except KeyboardInterrupt:
        rc = 0
    sys.exit(rc)


if __name__ == "__main__":
    main()
