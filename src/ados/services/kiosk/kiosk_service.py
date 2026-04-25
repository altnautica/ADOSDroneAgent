"""HDMI kiosk service: Chromium under cage, pointed at the local HUD.

HDMI + gamepad should deliver standalone flight with no phone
required. This service owns the HDMI output.

Lifecycle:
1. Probe `/dev/dri/card0`. If absent, the Pi has no HDMI sink connected
   (or the DRM driver did not bind). Log clearly and exit 0 so systemd
   does not churn restarting. Rule 26: the rest of the ground station
   keeps working even without HDMI.
2. Resolve target URL via config -> env var -> default chain.
3. Launch `cage -- chromium-browser --kiosk ... <url>` as a child
   process. cage is a Wayland single-app compositor, lightest option
   on Pi 4B and the one the setup image ships.
4. Supervise the child. On exit, backoff-restart. Five crashes in 60
   seconds flips to ERROR and we stop restarting so systemd can apply
   its own service-level retry.
5. On SIGTERM: send SIGTERM to cage, wait 10 s for graceful exit,
   SIGKILL if it is still up.

Not in scope:
- Bundling a GCS build. The URL is assumed to be served elsewhere.
- Serving the GCS dev server.
- `?layer=minimal` render path. That is a GCS concern (Flutes wave).
"""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import time
from pathlib import Path
from typing import Any

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger

log = get_logger("kiosk.kiosk_service")

_DRM_CARD_PATH = Path("/dev/dri/card0")

_DEFAULT_URL = "http://localhost:4000/hud"
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


def _hdmi_present() -> bool:
    """True when the DRM card node exists.

    We do not try to detect a connected monitor. `/dev/dri/card0` is the
    kernel side of the KMS driver. If HDMI hardware is missing entirely
    (headless image, DRM driver not loaded) the node is absent and we
    cleanly skip the kiosk.
    """
    return _DRM_CARD_PATH.exists()


def _get_kiosk_config(config: Any) -> tuple[str | None, bool | None]:
    """Return (target_url, minimal_layer) from config, if present.

    The Pydantic config model does not yet declare a `ground_station`
    section. We access it defensively so callers work on both old and
    new config shapes. Either field may be None when unset.
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


def _build_chromium_argv(url: str) -> list[str]:
    """Full argv for `cage -- chromium-browser ...`.

    We invoke `chromium-browser`. On Debian-based Raspberry Pi OS that
    is the package binary. cage handles the Wayland compositor; we
    ask Chromium to use Wayland + EGL for hardware acceleration.
    """
    return [
        "cage",
        "--",
        "chromium-browser",
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


class KioskSupervisor:
    """Spawn and supervise the cage + Chromium child process."""

    def __init__(self, argv: list[str]) -> None:
        self._argv = argv
        self._proc: asyncio.subprocess.Process | None = None
        self._stop = asyncio.Event()
        self._crash_times: list[float] = []

    def request_stop(self) -> None:
        self._stop.set()

    async def _spawn(self) -> asyncio.subprocess.Process:
        log.info("kiosk_spawning", argv=self._argv)
        return await asyncio.create_subprocess_exec(
            *self._argv,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
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
                except asyncio.TimeoutError:
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
        # nothing matched, which is fine.
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
            except asyncio.TimeoutError:
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
            except asyncio.TimeoutError:
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

    argv = _build_chromium_argv(url)
    supervisor = KioskSupervisor(argv)

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
