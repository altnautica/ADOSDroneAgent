"""Demo mode OTA updater: simulates the update flow without touching disk."""

from __future__ import annotations

import asyncio
from enum import StrEnum

from ados.core.logging import get_logger
from ados.services.ota.downloader import DownloadProgress, DownloadState
from ados.services.ota.manifest import UpdateManifest

log = get_logger("ota-demo")

FAKE_MANIFEST = UpdateManifest(
    version="99.0.0",
    channel="demo",
    published_at="2099-01-01T00:00:00Z",
    download_url="https://github.com/altnautica/ADOSDroneAgent/releases/download/v99.0.0/ados_drone_agent-99.0.0-py3-none-any.whl",
    file_size=52_428_800,
    sha256="0" * 64,
    changelog="Demo update: this is a simulated update for testing.",
    release_url="https://github.com/altnautica/ADOSDroneAgent/releases/tag/v99.0.0",
)


class DemoUpdateState(StrEnum):
    """Demo update states."""

    IDLE = "idle"
    CHECKING = "checking"
    DOWNLOADING = "downloading"
    INSTALLING = "installing"
    DONE = "done"


class DemoOtaUpdater:
    """Simulates the OTA update flow for demo/testing purposes.

    After 30 seconds, 'finds' a fake update v99.0.0 and simulates
    a download at 10% every 2 seconds. Never actually installs anything.
    """

    def __init__(self) -> None:
        self._state = DemoUpdateState.IDLE
        self._progress = DownloadProgress()
        self._manifest: UpdateManifest | None = None

    @property
    def state(self) -> DemoUpdateState:
        return self._state

    @property
    def manifest(self) -> UpdateManifest | None:
        return self._manifest

    @property
    def download_progress(self) -> DownloadProgress:
        return self._progress

    async def run(self) -> None:
        """Simulate the full OTA lifecycle."""
        log.info("demo_ota_started, will find update in 30s")
        await asyncio.sleep(30)

        # "Find" an update
        self._state = DemoUpdateState.CHECKING
        log.info("demo_checking_for_update")
        await asyncio.sleep(2)

        self._manifest = FAKE_MANIFEST
        log.info("demo_update_found", version=FAKE_MANIFEST.version)

        # Simulate download
        self._state = DemoUpdateState.DOWNLOADING
        self._progress = DownloadProgress(
            state=DownloadState.DOWNLOADING,
            total_bytes=FAKE_MANIFEST.file_size,
        )

        chunk_size = FAKE_MANIFEST.file_size // 10
        for i in range(10):
            await asyncio.sleep(2)
            self._progress.bytes_downloaded = min(
                (i + 1) * chunk_size, FAKE_MANIFEST.file_size
            )
            self._progress.speed_bps = chunk_size / 2.0
            remaining = FAKE_MANIFEST.file_size - self._progress.bytes_downloaded
            if self._progress.speed_bps > 0:
                self._progress.eta_seconds = remaining / self._progress.speed_bps
            log.info(
                "demo_download_progress",
                percent=round(self._progress.percent(), 1),
            )

        self._progress.state = DownloadState.COMPLETED
        self._progress.speed_bps = 0.0
        self._progress.eta_seconds = 0.0
        log.info("demo_download_complete")

        # Simulate install
        self._state = DemoUpdateState.INSTALLING
        await asyncio.sleep(3)

        self._state = DemoUpdateState.DONE
        log.info("demo_ota_complete, no actual changes made")

    def get_status(self) -> dict:
        """Return demo OTA status for API responses."""
        result: dict = {
            "state": self._state.value,
            "demo_mode": True,
            "current_version": "0.1.0",
            "channel": "demo",
            "github_repo": "altnautica/ADOSDroneAgent",
            "last_check": "",
            "previous_version": "",
            "error": "",
        }

        if self._manifest:
            result["pending_update"] = {
                "version": self._manifest.version,
                "channel": self._manifest.channel,
                "changelog": self._manifest.changelog,
                "release_url": self._manifest.release_url,
            }

        result["download"] = {
            "state": self._progress.state.value,
            "percent": round(self._progress.percent(), 1),
            "bytes_downloaded": self._progress.bytes_downloaded,
            "total_bytes": self._progress.total_bytes,
            "speed_bps": 0,
            "eta_seconds": 0,
        }

        return result
