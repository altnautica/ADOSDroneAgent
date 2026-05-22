"""Script executor — run Python scripts as sandboxed subprocesses."""

from __future__ import annotations

import asyncio
import json
import os
import re
import tempfile
import uuid
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from enum import StrEnum
from pathlib import Path
from typing import TYPE_CHECKING

from ados.core.logging import get_logger
from ados.core.paths import SCRIPTS_DIR

if TYPE_CHECKING:
    from ados.core.config import ScriptsConfig
    from ados.services.scripting.executor import CommandExecutor

log = get_logger("scripting.script_runner")

# Reverse-DNS-style script id. Generated server-side as a 12-char hex
# so we never trust client-supplied ids on the filesystem.
_SAVED_ID_RE = re.compile(r"^[a-f0-9]{12}$")

# Hard caps on the persistent library. A paired operator who buggies
# out (or a malicious caller who slipped through the auth gate) cannot
# fill /var/ados/scripts/ until the partition wedges agent operation.
_MAX_SAVED_SCRIPTS = 256
_MAX_SCRIPT_CONTENT_BYTES = 256 * 1024  # 256 KiB


@dataclass
class SavedScript:
    """Persisted script source as the dashboard / GCS see it. Mirrors
    Mission Control's ``ScriptInfo`` TypeScript interface verbatim so
    the API response is consumable without an adapter layer."""

    id: str
    name: str
    content: str
    suite: str | None = None
    lastModified: str = ""


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

    # ---- Persistent script library ------------------------------------
    # The runner above tracks running and finished executions in
    # memory. The library below persists script source on disk under
    # SCRIPTS_DIR so the GCS Scripts tab can save / list / re-run them
    # across agent restarts. Each saved script is a {id}.json manifest
    # carrying the source plus metadata; the runner materialises a
    # temp .py file at run-time so subprocess can exec it.

    @staticmethod
    def _saved_path(script_id: str) -> Path:
        return SCRIPTS_DIR / f"{script_id}.json"

    @classmethod
    def _read_saved(cls, script_id: str) -> SavedScript | None:
        if not _SAVED_ID_RE.fullmatch(script_id):
            return None
        path = cls._saved_path(script_id)
        if not path.is_file():
            return None
        try:
            raw = path.read_text(encoding="utf-8")
            data = json.loads(raw)
            return SavedScript(
                id=str(data.get("id", script_id)),
                name=str(data.get("name", "")),
                content=str(data.get("content", "")),
                suite=data.get("suite"),
                lastModified=str(data.get("lastModified", "")),
            )
        except (OSError, json.JSONDecodeError) as exc:
            log.warning("saved_script_unreadable", script_id=script_id, error=str(exc))
            return None

    def list_saved_scripts(self) -> list[SavedScript]:
        """Return every persisted script as a SavedScript record."""
        out: list[SavedScript] = []
        try:
            SCRIPTS_DIR.mkdir(parents=True, exist_ok=True)
            for path in sorted(SCRIPTS_DIR.glob("*.json")):
                script_id = path.stem
                saved = self._read_saved(script_id)
                if saved is not None:
                    out.append(saved)
        except OSError as exc:
            log.warning("scripts_dir_unreadable", error=str(exc))
        return out

    def get_saved_script(self, script_id: str) -> SavedScript | None:
        return self._read_saved(script_id)

    def save_script(
        self,
        name: str,
        content: str,
        suite: str | None = None,
    ) -> SavedScript:
        """Create or update a saved script. The id is server-assigned
        on first save; subsequent saves with the same name replace the
        existing record in place to keep the (name -> id) mapping
        stable for the dashboard."""
        name = name.strip()
        if not name:
            raise RuntimeError("script name required")
        # Bound the wire payload so a buggy caller (or one that
        # slipped through the auth gate) cannot push arbitrary-size
        # blobs and wedge the disk. Sized in bytes against the
        # UTF-8 encoding rather than character count.
        content_bytes = content.encode("utf-8")
        if len(content_bytes) > _MAX_SCRIPT_CONTENT_BYTES:
            raise RuntimeError(
                f"script content exceeds {_MAX_SCRIPT_CONTENT_BYTES} bytes",
            )
        SCRIPTS_DIR.mkdir(parents=True, exist_ok=True)
        # Reuse the existing id when the operator saves under the
        # same name; treat name as the natural key for the dashboard
        # while keeping the on-disk file keyed by stable id.
        existing = next(
            (s for s in self.list_saved_scripts() if s.name == name),
            None,
        )
        # Hard ceiling on library size. We only enforce on net-new
        # saves so an in-place update of an existing record always
        # succeeds; the library can never grow past the cap.
        if existing is None and len(self.list_saved_scripts()) >= _MAX_SAVED_SCRIPTS:
            raise RuntimeError(
                f"script library full (max {_MAX_SAVED_SCRIPTS} scripts)",
            )
        script_id = existing.id if existing else uuid.uuid4().hex[:12]
        record = SavedScript(
            id=script_id,
            name=name,
            content=content,
            suite=suite,
            lastModified=datetime.now(timezone.utc).isoformat(),
        )
        path = self._saved_path(script_id)
        # Atomic write: tmp + rename keeps a half-written file from
        # ever being read by the list call mid-update.
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(asdict(record), indent=2), encoding="utf-8")
        os.replace(tmp, path)
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
        log.info("script_saved", script_id=script_id, name=name)
        return record

    def delete_script(self, script_id: str) -> bool:
        """Remove a saved script. Returns False when the id is malformed
        or the record was already gone — never raises."""
        if not _SAVED_ID_RE.fullmatch(script_id):
            return False
        path = self._saved_path(script_id)
        try:
            path.unlink()
        except FileNotFoundError:
            return False
        except OSError as exc:
            log.warning("script_delete_failed", script_id=script_id, error=str(exc))
            return False
        log.info("script_deleted", script_id=script_id)
        return True

    def start_saved_script(self, script_id: str) -> str:
        """Resolve a saved script id to its content, write a temp .py
        file, and queue it through ``start_script``. Returns the
        execution id (distinct from the saved id)."""
        saved = self.get_saved_script(script_id)
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
