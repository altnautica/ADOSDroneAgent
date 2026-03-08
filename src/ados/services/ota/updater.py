"""OTA update orchestrator: check, download, verify, install."""

from __future__ import annotations

import asyncio
import shutil
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

SLOT_A_PATH = "/ados/slot-a"
SLOT_B_PATH = "/ados/slot-b"
DOWNLOAD_DIR = "/var/ados/downloads"


class UpdateState(StrEnum):
    """OTA update lifecycle states."""

    IDLE = "idle"
    CHECKING = "checking"
    DOWNLOADING = "downloading"
    VERIFYING = "verifying"
    INSTALLING = "installing"
    REBOOTING = "rebooting"
    FAILED = "failed"


class OtaUpdater:
    """Orchestrates the full OTA update pipeline."""

    def __init__(
        self,
        config: OtaConfig,
        checker: UpdateChecker,
        downloader: UpdateDownloader,
        rollback: RollbackManager,
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

    async def check(self) -> UpdateManifest | None:
        """Manually trigger an update check."""
        self._state = UpdateState.CHECKING
        self._error = ""
        log.info("manual_check_triggered")

        manifest = await self._checker.check_for_update(self._current_version)
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

        # Verify hash
        self._state = UpdateState.VERIFYING
        if not verify_sha256(filepath, manifest.sha256):
            self._state = UpdateState.FAILED
            self._error = "SHA-256 verification failed"
            log.error("verification_failed")
            return False

        self._downloaded_path = filepath
        self._state = UpdateState.IDLE
        log.info("download_verified", path=filepath)
        return True

    async def install(self) -> bool:
        """Install a downloaded and verified update to the standby partition."""
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

        # Determine standby partition path
        active_slot = self._rollback.get_active_slot()
        if active_slot.slot_name == "a":
            standby_path = Path(SLOT_B_PATH)
        else:
            standby_path = Path(SLOT_A_PATH)

        log.info(
            "installing_update",
            version=manifest.version,
            target=str(standby_path),
        )

        try:
            # In a real system, this would extract the update bundle
            # to the standby partition. Here we simulate the process.
            standby_path.mkdir(parents=True, exist_ok=True)

            # Copy update bundle to standby
            target_file = standby_path / "update.bin"
            shutil.copy2(self._downloaded_path, str(target_file))

            # Migrate config from active slot
            active_path = Path(SLOT_A_PATH if active_slot.slot_name == "a" else SLOT_B_PATH)
            active_config = active_path / "config.yaml"
            if active_config.exists():
                shutil.copy2(str(active_config), str(standby_path / "config.yaml"))

        except OSError as exc:
            self._state = UpdateState.FAILED
            self._error = f"Installation failed: {exc}"
            log.error("install_failed", error=str(exc))
            return False

        # Mark standby as bootable
        self._rollback.prepare_standby(manifest.version)

        self._state = UpdateState.IDLE
        log.info("install_complete", version=manifest.version)
        return True

    async def activate(self) -> bool:
        """Activate the standby partition for next boot."""
        success = self._rollback.activate_standby()
        if success:
            self._state = UpdateState.REBOOTING
            log.info("update_activated, reboot required")
        else:
            self._error = "Failed to activate standby slot"
            self._state = UpdateState.FAILED
        return success

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
                    await self.install()

            await asyncio.sleep(interval)

    def get_status(self) -> dict:
        """Return current update state for API responses."""
        result: dict = {
            "state": self._state.value,
            "current_version": self._current_version,
            "error": self._error,
        }

        if self._pending_manifest:
            result["pending_update"] = {
                "version": self._pending_manifest.version,
                "channel": self._pending_manifest.channel,
                "file_size": self._pending_manifest.file_size,
                "changelog": self._pending_manifest.changelog,
                "requires_reboot": self._pending_manifest.requires_reboot,
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

        result["slots"] = self._rollback.get_status()

        return result
