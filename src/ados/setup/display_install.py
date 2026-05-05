"""Subprocess job tracker for the wizard's display-install step.

The wizard's `Local display` step lets the operator pick a panel and
trigger ``scripts/drivers/install-display-overlay.sh`` from the
browser. The shell script needs root and can take 30+ seconds on a
fresh box (apt-get pulls device-tree-compiler the first time), so it
runs as a tracked background subprocess. The wizard polls a status
endpoint at 1-2 Hz while the job runs and renders the trailing log
lines so the operator sees progress.

Job lifecycle: ``queued`` -> ``running`` -> ``done`` (rc 0) or
``failed`` (rc != 0). Only one job runs at a time per agent process;
concurrent install requests get a 409 from the route. Job state lives
in a module-level dict and is forgotten across agent restart — fine
because the install script is idempotent.

The installer requires root. The agent normally runs as root via
systemd; on a dev box where it doesn't, the subprocess call will
return a non-zero exit code and the job ends ``failed``. The route
wraps ``install-display-overlay.sh`` directly without a sudo prefix
so a non-root install path produces a clean failure rather than a
sudo password prompt that would hang the wizard.
"""

from __future__ import annotations

import asyncio
import os
import uuid
from collections import deque
from datetime import datetime, timezone
from pathlib import Path
from typing import Deque

from ados.core.logging import get_logger
from ados.core.paths import DISPLAY_CONF_PATH

log = get_logger("setup.display_install")


# Trailing-log ring buffer cap. 40 lines is enough to show the last
# few apt + dtc + install lines without ballooning memory if a
# verbose subprocess writes thousands of lines.
LOG_TAIL_CAP = 40

# Path resolution helpers. The shell driver lives next to install.sh
# under scripts/drivers/. The agent installs itself under
# /opt/ados/source/ via curl-pipe, but the dev path (running from a
# git checkout) needs to walk up from this module's location.
_SCRIPT_NAME = "install-display-overlay.sh"


def _resolve_driver_script() -> Path | None:
    """Locate the LCD-overlay installer relative to the running agent."""
    candidates = [
        Path("/opt/ados/source/scripts/drivers") / _SCRIPT_NAME,
        Path("/usr/local/share/ados/scripts/drivers") / _SCRIPT_NAME,
        Path(__file__).resolve().parents[3] / "scripts" / "drivers" / _SCRIPT_NAME,
    ]
    for path in candidates:
        if path.exists() and os.access(path, os.X_OK):
            return path
    return None


class _JobHandle:
    """In-memory state for a single install run."""

    __slots__ = (
        "job_id",
        "display_id",
        "status",
        "started_at",
        "finished_at",
        "exit_code",
        "log_tail",
        "_proc",
    )

    def __init__(self, *, job_id: str, display_id: str) -> None:
        self.job_id = job_id
        self.display_id = display_id
        self.status: str = "queued"
        self.started_at: str = _now_iso()
        self.finished_at: str | None = None
        self.exit_code: int | None = None
        self.log_tail: Deque[str] = deque(maxlen=LOG_TAIL_CAP)
        self._proc: asyncio.subprocess.Process | None = None

    def to_dict(self) -> dict[str, object]:
        return {
            "job_id": self.job_id,
            "display_id": self.display_id,
            "status": self.status,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "exit_code": self.exit_code,
            "log_tail": list(self.log_tail),
        }


# Module-level state. Single-job model means `_active_job_id` either
# names the job that is currently queued/running or is None. Finished
# jobs stay in `_jobs` so the wizard can reload the page and still see
# the result of a recent install.
_jobs: dict[str, _JobHandle] = {}
_active_job_id: str | None = None
_lock = asyncio.Lock()


def _now_iso() -> str:
    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )


def get_job(job_id: str) -> _JobHandle | None:
    return _jobs.get(job_id)


def latest_job() -> _JobHandle | None:
    """Return the most recently started job, or None when no jobs exist."""
    if not _jobs:
        return None
    # Jobs are inserted in start order; rely on insertion order
    # preservation which is guaranteed for ``dict`` since 3.7.
    return next(reversed(_jobs.values()))


async def start_install(display_id: str) -> _JobHandle:
    """Spawn the installer as a tracked background subprocess.

    Raises ``RuntimeError`` when another install job is already
    queued or running — the route translates this into a 409.
    Raises ``FileNotFoundError`` when the installer script cannot be
    located on disk.
    """
    global _active_job_id
    async with _lock:
        if _active_job_id is not None:
            active = _jobs.get(_active_job_id)
            if active and active.status in ("queued", "running"):
                raise RuntimeError(
                    f"another install job ({_active_job_id}) is {active.status}"
                )
        script = _resolve_driver_script()
        if script is None:
            raise FileNotFoundError(
                "install-display-overlay.sh not found on disk. "
                "Re-run install.sh to refresh /opt/ados/source/."
            )
        job_id = uuid.uuid4().hex[:12]
        handle = _JobHandle(job_id=job_id, display_id=display_id)
        _jobs[job_id] = handle
        _active_job_id = job_id
        # Kick off the subprocess outside the lock — the lock guards
        # only the single-job invariant.
    asyncio.create_task(_run_job(handle, script, display_id))
    return handle


async def _run_job(
    handle: _JobHandle, script: Path, display_id: str
) -> None:
    """Drive a single install run end-to-end."""
    global _active_job_id
    handle.status = "running"
    handle.log_tail.append(f"[{_now_iso()}] starting {script} --display {display_id}")
    try:
        proc = await asyncio.create_subprocess_exec(
            str(script),
            "--display",
            display_id,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        handle._proc = proc
        assert proc.stdout is not None
        async for raw in proc.stdout:
            try:
                text = raw.decode("utf-8", errors="replace").rstrip("\n")
            except Exception:  # noqa: BLE001
                text = "<undecodable line>"
            handle.log_tail.append(text)
        rc = await proc.wait()
        handle.exit_code = rc
        handle.status = "done" if rc == 0 else "failed"
        handle.finished_at = _now_iso()
        handle.log_tail.append(f"[{handle.finished_at}] exit code {rc}")
    except Exception as exc:  # noqa: BLE001
        handle.status = "failed"
        handle.exit_code = -1
        handle.finished_at = _now_iso()
        handle.log_tail.append(f"[{handle.finished_at}] dispatcher error: {exc}")
        log.warning("display_install_dispatcher_error", error=str(exc))
    finally:
        async with _lock:
            if _active_job_id == handle.job_id:
                _active_job_id = None


def write_skip_marker() -> None:
    """Write a minimal /etc/ados/display.conf for the explicit-skip path.

    Sets ``display_id=none`` so the wizard step transitions to the
    ``optional`` state and the heartbeat assembler does not emit a
    peripherals[] entry. Idempotent — safe to call repeatedly.
    """
    DISPLAY_CONF_PATH.parent.mkdir(parents=True, exist_ok=True)
    DISPLAY_CONF_PATH.write_text(
        "# Written by the wizard's display step (operator skipped).\n"
        "display_id=none\n"
        "has_touch=false\n"
        "framebuffer_path=\n"
        f"skipped_at={_now_iso()}\n"
    )
    DISPLAY_CONF_PATH.chmod(0o644)


# Convenience for tests — clears job state without restarting the
# agent. Not part of the public API.
def _reset_for_tests() -> None:
    global _active_job_id
    _jobs.clear()
    _active_job_id = None
