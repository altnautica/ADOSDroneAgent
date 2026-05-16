"""Sandboxed vendor binary spawn for plugins.

Plugins that ship a vendor binary (for example an OpenVINS C++ shim or
a vendor camera control utility) declare the binary basenames they
need to exec in the manifest's ``agent.subprocess_spawn`` allowlist.
At runtime the plugin calls ``ctx.process.spawn(basename, args, env)``
which routes through the supervisor IPC and ultimately lands here.

This module is the enforcement seam:

* Resolves the binary basename against the plugin's install directory
  (``<install_dir>/vendor/<basename>`` canonical layout).
* Rejects any basename not on the manifest allowlist with
  :class:`AllowlistViolation`.
* Rejects shell-meta in basenames and any path traversal attempt.
* Confirms the resolved file is regular and executable.
* Spawns the child via :func:`subprocess.Popen`. The child inherits
  the plugin runner's cgroup membership through normal process-tree
  inheritance (the runner already lives inside
  ``ados-plugins.slice`` per the systemd unit definition).
* Drops file-descriptor inheritance with ``close_fds=True`` and starts
  the child in a new session so a stray signal in the runner does not
  reach the vendor binary uncontrolled.

The returned :class:`SpawnedProcess` exposes the standard streams plus
``terminate`` / ``wait`` / ``poll``. Plugins are expected to read or
discard stdout / stderr promptly; the OS pipe buffer is small.

The runner that ran ``ctx.process.spawn`` keeps a reference to the
:class:`SpawnedProcess` so the supervisor can terminate the child when
the plugin transitions to STOPPED.
"""

from __future__ import annotations

import os
import shlex
import signal
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import IO

from ados.core.logging import get_logger
from ados.plugins.errors import PluginError

log = get_logger("plugins.process_sandbox")


VENDOR_DIR_NAME = "vendor"
"""Canonical subdirectory under the plugin install root where vendor
binaries live. Plugins package their vendor blobs at
``agent/vendor/<basename>`` and the archive extractor lays them out at
``<install_dir>/vendor/<basename>`` after install."""

DEFAULT_TIMEOUT_S = 30.0
"""Default upper bound on the time we wait for a spawned process to
respond to ``terminate``. Vendor binaries that ignore SIGTERM get a
SIGKILL after this elapses."""


class AllowlistViolation(PluginError):
    """Raised when a plugin tries to spawn a binary that is not on its
    manifest ``agent.subprocess_spawn`` allowlist.

    Carries the offending basename and the plugin id so the audit log
    captures the attempt verbatim.
    """

    def __init__(self, plugin_id: str, basename: str) -> None:
        super().__init__(
            f"plugin {plugin_id} attempted to spawn {basename!r} "
            "which is not on the manifest subprocess_spawn allowlist"
        )
        self.plugin_id = plugin_id
        self.basename = basename


class SpawnError(PluginError):
    """Raised when the spawn itself fails (binary missing, not
    executable, exec returned ENOENT, etc.). Distinct from
    :class:`AllowlistViolation` so the audit can tell intent (denied)
    from operational failure (broken)."""


@dataclass
class SpawnedProcess:
    """Handle returned to the plugin runner for a spawned vendor binary.

    The handle wraps a :class:`subprocess.Popen` and exposes the
    subset plugin authors need. ``terminate`` first sends SIGTERM and
    falls back to SIGKILL after ``DEFAULT_TIMEOUT_S`` so a vendor
    binary that ignores polite shutdown cannot wedge the plugin
    teardown.
    """

    plugin_id: str
    basename: str
    pid: int
    _popen: subprocess.Popen

    @property
    def stdin(self) -> IO[bytes] | None:
        return self._popen.stdin

    @property
    def stdout(self) -> IO[bytes] | None:
        return self._popen.stdout

    @property
    def stderr(self) -> IO[bytes] | None:
        return self._popen.stderr

    def poll(self) -> int | None:
        """Return the exit code, or None if still running."""
        return self._popen.poll()

    def wait(self, timeout: float | None = None) -> int:
        """Block until the child exits; return its exit code."""
        return self._popen.wait(timeout=timeout)

    def terminate(self, *, timeout: float = DEFAULT_TIMEOUT_S) -> int:
        """Politely stop the child. SIGTERM, wait up to ``timeout``,
        then SIGKILL. Returns the exit code."""
        if self._popen.poll() is not None:
            return self._popen.returncode
        try:
            self._popen.terminate()
        except ProcessLookupError:
            return self._popen.returncode or 0
        try:
            return self._popen.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            log.warning(
                "plugin_process_terminate_timeout",
                plugin_id=self.plugin_id,
                basename=self.basename,
                pid=self.pid,
            )
            try:
                self._popen.kill()
            except ProcessLookupError:
                return self._popen.returncode or 0
            return self._popen.wait(timeout=5.0)


def _is_safe_basename(basename: str) -> bool:
    """Reject anything that is not a plain basename.

    The manifest allowlist stores plain basenames. We also reject any
    basename containing path separators, ``..``, or shell-meta so a
    crafted allowlist entry cannot smuggle a traversal into the spawn
    resolution.
    """
    if not basename:
        return False
    if "/" in basename or "\\" in basename:
        return False
    if basename in (".", "..") or basename.startswith("."):
        # Reserve leading-dot names for future use (none of the
        # existing extensions ship dotfiles as vendor binaries).
        return False
    # Reject any shell-meta. shlex.quote will wrap such names in quotes
    # but the safer choice is to refuse them up front.
    bad = set("\0\n\r\t ;&|<>$`\"'(){}[]*?!")
    if any(ch in bad for ch in basename):
        return False
    return True


def resolve_binary(install_dir: Path, basename: str) -> Path:
    """Resolve the vendor binary path inside ``install_dir``.

    The canonical layout is ``<install_dir>/vendor/<basename>``. The
    return value is fully resolved (symlinks followed) and its parent
    must remain inside the plugin's vendor directory; we reject any
    path whose resolved location escapes the plugin tree.
    """
    if not _is_safe_basename(basename):
        raise SpawnError(f"unsafe binary basename: {basename!r}")
    vendor_root = (install_dir / VENDOR_DIR_NAME).resolve()
    candidate = (vendor_root / basename).resolve()
    if vendor_root not in candidate.parents and candidate.parent != vendor_root:
        raise SpawnError(
            f"resolved binary {candidate} escapes vendor root {vendor_root}"
        )
    if not candidate.exists():
        raise SpawnError(f"vendor binary not found: {candidate}")
    if not candidate.is_file():
        raise SpawnError(f"vendor entry is not a regular file: {candidate}")
    if not os.access(candidate, os.X_OK):
        raise SpawnError(f"vendor binary not executable: {candidate}")
    return candidate


def spawn(
    *,
    plugin_id: str,
    install_dir: Path,
    allowlist: list[str] | tuple[str, ...] | frozenset[str],
    basename: str,
    args: list[str] | None = None,
    env: dict[str, str] | None = None,
    cwd: Path | None = None,
    stdin: int = subprocess.PIPE,
    stdout: int = subprocess.PIPE,
    stderr: int = subprocess.PIPE,
) -> SpawnedProcess:
    """Spawn ``basename`` with allowlist enforcement.

    Raises:
        AllowlistViolation: basename is not on the manifest allowlist.
        SpawnError: binary is missing, not executable, or exec fails.
    """
    allow = frozenset(allowlist or ())
    if basename not in allow:
        log.warning(
            "plugin_process_spawn_denied",
            plugin_id=plugin_id,
            basename=basename,
            allowlist_size=len(allow),
        )
        raise AllowlistViolation(plugin_id=plugin_id, basename=basename)

    binary_path = resolve_binary(install_dir, basename)

    # Quote args defensively when logging so a noisy audit trail does
    # not splice meta-chars from caller input.
    safe_args = [shlex.quote(a) for a in (args or [])]
    log.info(
        "plugin_process_spawn",
        plugin_id=plugin_id,
        basename=basename,
        path=str(binary_path),
        args=safe_args,
    )

    # Build a clean env: start from caller-provided env, do not
    # inherit the entire host environment. The plugin manifest's
    # ``agent.env`` block is merged earlier by the IPC handler; what
    # arrives here is the final exec env.
    final_env: dict[str, str] = {}
    if env is not None:
        final_env.update({str(k): str(v) for k, v in env.items()})
    # Always set PATH to a minimal value so the child cannot rely on
    # ambient PATH lookup. The vendor binary is exec'd by absolute
    # path; PATH is only there for any tools the binary itself spawns.
    final_env.setdefault("PATH", "/usr/local/bin:/usr/bin:/bin")

    try:
        popen = subprocess.Popen(
            [str(binary_path), *(args or [])],
            stdin=stdin,
            stdout=stdout,
            stderr=stderr,
            env=final_env,
            cwd=str(cwd) if cwd is not None else str(install_dir),
            close_fds=True,
            # New session so a SIGINT delivered to the runner's
            # process group does not propagate uncontrolled to the
            # vendor binary; the runner explicitly terminates the
            # SpawnedProcess on plugin stop.
            start_new_session=True,
        )
    except (OSError, ValueError) as exc:
        raise SpawnError(
            f"spawn failed for {basename}: {exc}"
        ) from exc

    return SpawnedProcess(
        plugin_id=plugin_id,
        basename=basename,
        pid=popen.pid,
        _popen=popen,
    )


def terminate_all(handles: list[SpawnedProcess]) -> None:
    """Best-effort terminate of every handle.

    Called from the plugin runner during shutdown. Each handle is
    attempted independently; one stuck child does not block the rest.
    """
    for h in list(handles):
        try:
            h.terminate()
        except (ProcessLookupError, OSError) as exc:
            log.warning(
                "plugin_process_terminate_error",
                plugin_id=h.plugin_id,
                basename=h.basename,
                pid=h.pid,
                error=str(exc),
            )


__all__ = [
    "AllowlistViolation",
    "SpawnError",
    "SpawnedProcess",
    "VENDOR_DIR_NAME",
    "DEFAULT_TIMEOUT_S",
    "resolve_binary",
    "spawn",
    "terminate_all",
]


# Internal: re-export signal for callers that want to inspect exit
# semantics (the supervisor records terminated-by-signal exit codes
# the same way systemd does, as 128 + signum).
_NEGATIVE_SIGNAL_OFFSET = 128


def signum_from_returncode(returncode: int) -> int | None:
    """If ``returncode`` indicates termination by signal, return the
    signal number; otherwise None.

    POSIX subprocess returncodes are negative when the child was
    terminated by a signal (Python convention). systemd reports
    ``128 + signum`` for the same event in service logs. Translate
    both shapes so audit logs are consistent.
    """
    if returncode < 0:
        return -returncode
    if returncode > _NEGATIVE_SIGNAL_OFFSET:
        candidate = returncode - _NEGATIVE_SIGNAL_OFFSET
        try:
            signal.Signals(candidate)
        except ValueError:
            return None
        return candidate
    return None
