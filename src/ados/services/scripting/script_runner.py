"""Script executor — run Python scripts as sandboxed subprocesses."""

from __future__ import annotations

import asyncio
import os
import tempfile
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import StrEnum
from pathlib import Path
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.core.paths import SCRIPTS_DIR
from ados.services.scripting import script_library
from ados.services.scripting.script_library import SavedScript  # re-export

if TYPE_CHECKING:
    from ados.core.config import ScriptsConfig
    from ados.services.scripting.executor import CommandExecutor

log = get_logger("scripting.script_runner")


class ScriptState(StrEnum):
    """Lifecycle states for a running script."""

    QUEUED = "queued"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"


@dataclass
class ScriptInfo:
    """Metadata and state for a script execution."""

    script_id: str
    filename: str
    state: ScriptState
    pid: int | None = None
    started_at: str = ""
    finished_at: str = ""
    output_lines: list[str] = field(default_factory=list)
    return_code: int | None = None


class ScriptRunner:
    """Manages concurrent script execution with resource limits.

    Scripts run as Python subprocesses with the ADOS_SDK_PORT env var set
    so they can connect back to the SDK server for drone control.
    """

    MAX_OUTPUT_LINES: int = 200

    def __init__(
        self,
        config: ScriptsConfig,
        executor: CommandExecutor,
        sdk_port: int = 8892,
    ) -> None:
        self._config = config
        self._executor = executor
        self._sdk_port = sdk_port
        self._scripts: dict[str, ScriptInfo] = {}
        self._processes: dict[str, asyncio.subprocess.Process] = {}

    def _active_count(self) -> int:
        return sum(
            1
            for s in self._scripts.values()
            if s.state in (ScriptState.QUEUED, ScriptState.RUNNING)
        )

    def start_script(self, path: str) -> str:
        """Queue a script for execution. Returns the script_id.

        Raises RuntimeError if max concurrent scripts reached or file not found.
        """
        if not os.path.isfile(path):
            raise RuntimeError(f"Script not found: {path}")

        if self._active_count() >= self._config.max_concurrent:
            raise RuntimeError(
                f"Max concurrent scripts reached ({self._config.max_concurrent})"
            )

        script_id = uuid.uuid4().hex[:12]
        info = ScriptInfo(
            script_id=script_id,
            filename=os.path.basename(path),
            state=ScriptState.QUEUED,
        )
        self._scripts[script_id] = info

        # Fire-and-forget the async launcher
        asyncio.get_event_loop().create_task(self._run_script(script_id, path))
        log.info("script_queued", script_id=script_id, path=path)
        return script_id

    def stop_script(self, script_id: str) -> bool:
        """Cancel a running script. Returns True if it was stopped."""
        info = self._scripts.get(script_id)
        if info is None:
            return False

        proc = self._processes.get(script_id)
        if proc is not None and proc.returncode is None:
            proc.terminate()
            info.state = ScriptState.CANCELLED
            info.finished_at = datetime.now(timezone.utc).isoformat()
            log.info("script_cancelled", script_id=script_id)
            return True

        return False

    def list_scripts(self) -> list[ScriptInfo]:
        """Return all known script executions (recent history)."""
        return list(self._scripts.values())

    def get_script(self, script_id: str) -> ScriptInfo | None:
        """Return info for a single script, or None."""
        return self._scripts.get(script_id)

    # ---- Persistent script library (delegates to script_library) -----
    # The runner above tracks running and finished executions in
    # memory. Library persistence (save / list / delete / read) lives
    # in ``script_library`` because the REST API process needs the
    # same disk-backed library, and that process does not own an
    # instantiated runner. The wrappers below keep existing in-tree
    # callers working without forcing them to refactor.

    @staticmethod
    def list_saved_scripts() -> list[SavedScript]:
        return script_library.list_saved_scripts()

    @staticmethod
    def get_saved_script(script_id: str) -> SavedScript | None:
        return script_library.get_saved_script(script_id)

    @staticmethod
    def save_script(
        name: str,
        content: str,
        suite: str | None = None,
    ) -> SavedScript:
        return script_library.save_script(name, content, suite)

    @staticmethod
    def delete_script(script_id: str) -> bool:
        return script_library.delete_script(script_id)

    # Older call sites in the test suite still use ``_saved_path`` to
    # probe filesystem invariants; expose it as a thin alias.
    @staticmethod
    def _saved_path(script_id: str) -> Path:
        return script_library._saved_path(script_id)

    def start_saved_script(self, script_id: str) -> str:
        """Resolve a saved script id to its content, write a temp .py
        file, and queue it through ``start_script``. Returns the
        execution id (distinct from the saved id)."""
        saved = script_library.get_saved_script(script_id)
        if saved is None:
            raise RuntimeError(f"saved script not found: {script_id}")
        # Materialise to a temp file the subprocess can exec. Cleanup
        # belongs to the subprocess lifecycle; the runner does not
        # delete the file because the script may still be running
        # when the route returns.
        SCRIPTS_DIR.mkdir(parents=True, exist_ok=True)
        runs_dir = SCRIPTS_DIR / "runs"
        runs_dir.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(
            mode="w",
            dir=runs_dir,
            prefix=f"{saved.id}-",
            suffix=".py",
            delete=False,
            encoding="utf-8",
        ) as fh:
            fh.write(saved.content)
            tmp_path = fh.name
        return self.start_script(tmp_path)

    async def _run_script(self, script_id: str, path: str) -> None:
        """Launch the script subprocess and track its output."""
        info = self._scripts[script_id]
        info.state = ScriptState.RUNNING
        info.started_at = datetime.now(timezone.utc).isoformat()

        # Materialised runs land under SCRIPTS_DIR/runs/ and are
        # transient — clean them up once the subprocess has exited so
        # a 5-minute-loop script does not leak hundreds of files into
        # /var/ados/scripts/runs/ across a long uptime.
        runs_root = SCRIPTS_DIR / "runs"
        cleanup_path: str | None = None
        try:
            if Path(path).resolve().is_relative_to(runs_root.resolve()):
                cleanup_path = path
        except (OSError, ValueError):
            cleanup_path = None

        env = os.environ.copy()
        env["ADOS_SDK_PORT"] = str(self._sdk_port)
        env["ADOS_SCRIPT_ID"] = script_id

        try:
            proc = await asyncio.create_subprocess_exec(
                "python3", path,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
                env=env,
            )
            info.pid = proc.pid
            self._processes[script_id] = proc
            log.info("script_started", script_id=script_id, pid=proc.pid)

            # Read output line by line
            if proc.stdout is not None:
                while True:
                    line = await proc.stdout.readline()
                    if not line:
                        break
                    decoded = line.decode("utf-8", errors="replace").rstrip()
                    info.output_lines.append(decoded)
                    if len(info.output_lines) > self.MAX_OUTPUT_LINES:
                        info.output_lines = info.output_lines[-self.MAX_OUTPUT_LINES:]

            await proc.wait()
            info.return_code = proc.returncode

            if info.state == ScriptState.CANCELLED:
                # Already marked cancelled by stop_script
                pass
            elif proc.returncode == 0:
                info.state = ScriptState.COMPLETED
            else:
                info.state = ScriptState.FAILED

            info.finished_at = datetime.now(timezone.utc).isoformat()
            log.info(
                "script_finished",
                script_id=script_id,
                state=info.state.value,
                return_code=proc.returncode,
            )
        except Exception as exc:
            info.state = ScriptState.FAILED
            info.finished_at = datetime.now(timezone.utc).isoformat()
            info.output_lines.append(f"Launch error: {exc}")
            log.error("script_launch_error", script_id=script_id, error=str(exc))
        finally:
            self._processes.pop(script_id, None)
            if cleanup_path is not None:
                try:
                    os.unlink(cleanup_path)
                except FileNotFoundError:
                    pass
                except OSError as exc:
                    log.warning(
                        "script_run_cleanup_failed",
                        path=cleanup_path,
                        error=str(exc),
                    )
