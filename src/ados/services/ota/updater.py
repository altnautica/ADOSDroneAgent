"""OTA update orchestrator: check, download, verify, install via pip."""

from __future__ import annotations

import asyncio
import json
import platform
import subprocess
from datetime import datetime, timezone
from enum import StrEnum
from pathlib import Path

from ados.core.config import OtaConfig
from ados.core.logging import get_logger
from ados.services.ota.checker import UpdateChecker
from ados.services.ota.downloader import DownloadProgress, UpdateDownloader
from ados.services.ota.manifest import UpdateManifest
from ados.services.ota.rollback import RollbackManager
from ados.services.ota.verifier import verify_sha256

log = get_logger("ota-updater")

DOWNLOAD_DIR = "/var/ados/downloads"
STATE_FILE = "/var/ados/ota-state.json"


class UpdateState(StrEnum):
    """OTA update lifecycle states."""

    IDLE = "idle"
    CHECKING = "checking"
    DOWNLOADING = "downloading"
    VERIFYING = "verifying"
    INSTALLING = "installing"
    RESTARTING = "restarting"
    FAILED = "failed"


class OtaUpdater:
    """Orchestrates the full OTA update pipeline using pip install."""

    def __init__(
        self,
        config: OtaConfig,
        checker: UpdateChecker,
        downloader: UpdateDownloader,
        rollback: RollbackManager | None = None,
        current_version: str = "0.1.0",
    ) -> None:
        self._config = config
        self._checker = checker
        self._downloader = downloader
        self._rollback = rollback
        self._current_version = current_version
        self._state = UpdateState.IDLE
        self._error: str = ""
        self._pending_manifest: UpdateManifest | None = None
        self._downloaded_path: str = ""
        self._last_check: str = ""
        self._previous_version: str = self._load_previous_version()

    @property
    def state(self) -> UpdateState:
        return self._state

    @property
    def error(self) -> str:
        return self._error

    @property
    def pending_manifest(self) -> UpdateManifest | None:
        return self._pending_manifest

    @property
    def download_progress(self) -> DownloadProgress:
        return self._downloader.progress

    def _load_previous_version(self) -> str:
        """Load previous version from OTA state file."""
        try:
            state_path = Path(STATE_FILE)
            if state_path.exists():
                data = json.loads(state_path.read_text())
                return data.get("previous_version", "")
        except (json.JSONDecodeError, OSError):
            pass
        return ""

    def _save_state(self, previous_version: str) -> None:
        """Write pre-update state to disk for rollback tracking."""
        state_path = Path(STATE_FILE)
        state_path.parent.mkdir(parents=True, exist_ok=True)
        state_path.write_text(json.dumps({
            "previous_version": previous_version,
            "updated_at": datetime.now(timezone.utc).isoformat(),
        }))

    async def check(self) -> UpdateManifest | None:
        """Manually trigger an update check."""
        self._state = UpdateState.CHECKING
        self._error = ""
        log.info("manual_check_triggered")

        manifest = await self._checker.check_for_update(self._current_version)
        self._last_check = datetime.now(timezone.utc).isoformat()

        if manifest:
            self._pending_manifest = manifest
            log.info("update_found", version=manifest.version)
        else:
            log.info("no_update_found")

        self._state = UpdateState.IDLE
        return manifest

    async def download_and_verify(self) -> bool:
        """Download and verify the pending update."""
        if not self._pending_manifest:
            self._error = "No pending update to download"
            log.warning("download_no_pending")
            return False

        manifest = self._pending_manifest

        # Download
        self._state = UpdateState.DOWNLOADING
        self._error = ""
        try:
            filepath = await self._downloader.download(manifest, DOWNLOAD_DIR)
        except Exception as exc:
            self._state = UpdateState.FAILED
            self._error = f"Download failed: {exc}"
            log.error("download_failed", error=str(exc))
            return False

        # Verify hash (skip if no SHA256 was available)
        self._state = UpdateState.VERIFYING
        if manifest.sha256:
            if not verify_sha256(filepath, manifest.sha256):
                self._state = UpdateState.FAILED
                self._error = "SHA-256 verification failed"
                log.error("verification_failed")
                # Clean up bad download
                try:
                    Path(filepath).unlink(missing_ok=True)
                except OSError:
                    pass
                return False
        else:
            log.warning("skipping_hash_verification", msg="no SHA256 in manifest")

        self._downloaded_path = filepath
        self._state = UpdateState.IDLE
        log.info("download_verified", path=filepath)
        return True

    async def install(self) -> bool:
        """Install the downloaded wheel via pip."""
        if not self._downloaded_path:
            self._error = "No verified download to install"
            log.warning("install_no_download")
            return False

        if not self._pending_manifest:
            self._error = "No pending manifest"
            return False

        self._state = UpdateState.INSTALLING
        self._error = ""
        manifest = self._pending_manifest

        log.info("installing_update", version=manifest.version, wheel=self._downloaded_path)

        # Save pre-update state for rollback
        self._save_state(self._current_version)

        # pip install the wheel
        pip_path = self._config.pip_path
        try:
            loop = asyncio.get_running_loop()
            result = await loop.run_in_executor(
                None,
                lambda: subprocess.run(
                    [pip_path, "install", "--no-deps", self._downloaded_path],
                    capture_output=True,
                    text=True,
                    timeout=120,
                ),
            )

            if result.returncode != 0:
                self._state = UpdateState.FAILED
                self._error = f"pip install failed: {result.stderr.strip()}"
                log.error("pip_install_failed", stderr=result.stderr.strip())
                return False

        except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
            self._state = UpdateState.FAILED
            self._error = f"Installation failed: {exc}"
            log.error("install_failed", error=str(exc))
            return False

        # Clean up the downloaded wheel
        try:
            Path(self._downloaded_path).unlink(missing_ok=True)
        except OSError:
            pass

        self._previous_version = self._current_version
        self._current_version = manifest.version
        self._downloaded_path = ""
        self._pending_manifest = None
        self._state = UpdateState.IDLE
        log.info("install_complete", version=manifest.version)
        return True

    async def restart_service(self) -> bool:
        """Restart the agent systemd service. On macOS, just log a message."""
        if platform.system() != "Linux":
            log.info("restart_skipped", msg="not on Linux, restart the agent manually")
            return False

        self._state = UpdateState.RESTARTING
        service = self._config.service_name
        log.info("restarting_service", service=service)

        try:
            loop = asyncio.get_running_loop()
            result = await loop.run_in_executor(
                None,
                lambda: subprocess.run(
                    ["systemctl", "restart", service],
                    capture_output=True,
                    text=True,
                    timeout=30,
                ),
            )

            if result.returncode != 0:
                log.warning("restart_failed", stderr=result.stderr.strip())
                return False

            return True

        except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
            log.warning("restart_error", error=str(exc))
            return False

    async def rollback(self, version: str | None = None) -> bool:
        """Rollback to a previous version by installing from PyPI."""
        target = version or self._previous_version
        if not target:
            self._error = "No previous version to rollback to"
            log.warning("rollback_no_version")
            return False

        self._state = UpdateState.INSTALLING
        self._error = ""
        log.info("rolling_back", target_version=target)

        pip_path = self._config.pip_path
        try:
            loop = asyncio.get_running_loop()
            result = await loop.run_in_executor(
                None,
                lambda: subprocess.run(
                    [pip_path, "install", f"ados-drone-agent=={target}"],
                    capture_output=True,
                    text=True,
                    timeout=120,
                ),
            )

            if result.returncode != 0:
                self._state = UpdateState.FAILED
                self._error = f"Rollback failed: {result.stderr.strip()}"
                log.error("rollback_pip_failed", stderr=result.stderr.strip())
                return False

        except (subprocess.TimeoutExpired, FileNotFoundError, OSError) as exc:
            self._state = UpdateState.FAILED
            self._error = f"Rollback failed: {exc}"
            log.error("rollback_failed", error=str(exc))
            return False

        self._previous_version = self._current_version
        self._current_version = target
        self._state = UpdateState.IDLE
        log.info("rollback_complete", version=target)

        # Restart service after rollback
        await self.restart_service()
        return True

    async def run(self) -> None:
        """Main service loop: periodic check, auto-install if configured."""
        interval = self._config.check_interval * 3600
        log.info("ota_service_started", interval_hours=self._config.check_interval)

        while True:
            manifest = await self.check()

            if manifest and self._config.auto_install:
                log.info("auto_install_enabled, starting download")
                ok = await self.download_and_verify()
                if ok:
                    installed = await self.install()
                    if installed:
                        await self.restart_service()

            await asyncio.sleep(interval)

    def get_status(self) -> dict:
        """Return current update state for API responses."""
        result: dict = {
            "state": self._state.value,
            "current_version": self._current_version,
            "channel": self._config.channel,
            "github_repo": self._config.github_repo,
            "last_check": self._last_check,
            "previous_version": self._previous_version,
            "error": self._error,
        }

        if self._pending_manifest:
            result["pending_update"] = {
                "version": self._pending_manifest.version,
                "channel": self._pending_manifest.channel,
                "file_size": self._pending_manifest.file_size,
                "changelog": self._pending_manifest.changelog,
                "release_url": self._pending_manifest.release_url,
            }

        progress = self._downloader.progress
        result["download"] = {
            "state": progress.state.value,
            "percent": round(progress.percent(), 1),
            "bytes_downloaded": progress.bytes_downloaded,
            "total_bytes": progress.total_bytes,
            "speed_bps": round(progress.speed_bps, 0),
            "eta_seconds": round(progress.eta_seconds, 0),
        }

        if self._rollback:
            result["slots"] = self._rollback.get_status()

        return result
