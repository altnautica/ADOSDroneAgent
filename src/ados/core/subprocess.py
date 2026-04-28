"""Shared subprocess helpers for service managers.

Centralises the timeout, decode, and structured-log scaffolding that
ground-station managers used to copy across modules. Two entry points:

* ``run_cmd`` for async callers. Uses ``asyncio.create_subprocess_exec``
  so it never blocks the event loop and integrates with cancellation.
* ``run_cmd_sync`` for managers that run inside synchronous code paths
  (systemd-init helpers, sync REST handlers). Wraps ``subprocess.run``
  with the same return shape as the async form.

Both forms return :class:`CmdResult`. Set ``check=True`` to raise
:class:`CmdError` on non-zero exit. Timeouts raise
``asyncio.TimeoutError`` from the async path and ``CmdTimeout`` from
the sync path.
"""

from __future__ import annotations

import asyncio
import shlex
import subprocess
from dataclasses import dataclass
from typing import Iterable

import structlog

log = structlog.get_logger(__name__)


@dataclass(frozen=True)
class CmdResult:
    """Outcome of a single subprocess invocation."""

    returncode: int
    stdout: str
    stderr: str

    @property
    def ok(self) -> bool:
        return self.returncode == 0


class CmdError(RuntimeError):
    """Raised when a command fails and ``check=True`` was set."""

    def __init__(self, cmd: list[str], result: CmdResult) -> None:
        self.cmd = cmd
        self.result = result
        super().__init__(
            f"command {shlex.join(cmd)!r} exited {result.returncode}: "
            f"{result.stderr.strip()}"
        )


class CmdTimeout(RuntimeError):
    """Raised by the sync helper when a command exceeds its timeout."""

    def __init__(self, cmd: list[str], timeout: float) -> None:
        self.cmd = cmd
        self.timeout = timeout
        super().__init__(
            f"command {shlex.join(cmd)!r} timed out after {timeout}s"
        )


def _decode(buf: bytes | None) -> str:
    if not buf:
        return ""
    return buf.decode(errors="replace")


async def run_cmd(
    cmd: Iterable[str],
    *,
    timeout: float = 5.0,
    check: bool = False,
    input_text: str | None = None,
    env: dict[str, str] | None = None,
) -> CmdResult:
    """Run a subprocess on the asyncio event loop.

    Returns a :class:`CmdResult` with the decoded stdout, stderr, and
    return code. Decoded as utf-8 with ``errors='replace'`` so binary
    junk on stderr never poisons the call site.

    Raises :class:`CmdError` on non-zero exit when ``check=True``.
    Raises :class:`asyncio.TimeoutError` when the process exceeds
    ``timeout``; the child is killed and reaped before the exception
    propagates.
    """
    cmd_list = list(cmd)
    proc = await asyncio.create_subprocess_exec(
        *cmd_list,
        stdin=asyncio.subprocess.PIPE if input_text is not None else None,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=env,
    )

    encoded_input = (
        input_text.encode("utf-8") if input_text is not None else None
    )
    try:
        stdout_bytes, stderr_bytes = await asyncio.wait_for(
            proc.communicate(input=encoded_input),
            timeout=timeout,
        )
    except asyncio.TimeoutError:
        proc.kill()
        try:
            await proc.wait()
        except (ProcessLookupError, asyncio.CancelledError):
            pass
        log.warning("run_cmd_timeout", cmd=cmd_list, timeout=timeout)
        raise

    result = CmdResult(
        returncode=proc.returncode or 0,
        stdout=_decode(stdout_bytes),
        stderr=_decode(stderr_bytes),
    )
    log.debug(
        "run_cmd",
        cmd=cmd_list,
        rc=result.returncode,
        stdout_len=len(result.stdout),
        stderr_len=len(result.stderr),
    )
    if check and not result.ok:
        raise CmdError(cmd_list, result)
    return result


def run_cmd_sync(
    cmd: Iterable[str],
    *,
    timeout: float = 5.0,
    check: bool = False,
    input_text: str | None = None,
    env: dict[str, str] | None = None,
) -> CmdResult:
    """Synchronous form of :func:`run_cmd`.

    For managers that run from sync code paths (systemd init, sync
    REST handlers) where adding ``await`` would force a wider refactor.
    Same return shape, same error semantics, with one difference:
    timeouts raise :class:`CmdTimeout` rather than
    ``asyncio.TimeoutError`` so callers do not need an asyncio import.
    """
    cmd_list = list(cmd)
    try:
        proc = subprocess.run(
            cmd_list,
            capture_output=True,
            timeout=timeout,
            input=input_text.encode("utf-8") if input_text else None,
            env=env,
            check=False,
        )
    except subprocess.TimeoutExpired:
        log.warning("run_cmd_sync_timeout", cmd=cmd_list, timeout=timeout)
        raise CmdTimeout(cmd_list, timeout) from None

    result = CmdResult(
        returncode=proc.returncode,
        stdout=_decode(proc.stdout),
        stderr=_decode(proc.stderr),
    )
    log.debug(
        "run_cmd_sync",
        cmd=cmd_list,
        rc=result.returncode,
        stdout_len=len(result.stdout),
        stderr_len=len(result.stderr),
    )
    if check and not result.ok:
        raise CmdError(cmd_list, result)
    return result


__all__ = [
    "CmdError",
    "CmdResult",
    "CmdTimeout",
    "run_cmd",
    "run_cmd_sync",
]
