"""Download OTA update bundles with resume support."""

from __future__ import annotations

import os
from collections.abc import Callable
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path

import httpx

from ados.core.logging import get_logger
from ados.services.ota.manifest import UpdateManifest

log = get_logger("ota-downloader")


class DownloadState(StrEnum):
    """Download lifecycle states."""

    IDLE = "idle"
    DOWNLOADING = "downloading"
    PAUSED = "paused"
    COMPLETED = "completed"
    FAILED = "failed"


@dataclass
class DownloadProgress:
    """Snapshot of download progress."""

    state: DownloadState = DownloadState.IDLE
    bytes_downloaded: int = 0
    total_bytes: int = 0
    speed_bps: float = 0.0
    eta_seconds: float = 0.0

    def percent(self) -> float:
        if self.total_bytes <= 0:
            return 0.0
        return min(100.0, (self.bytes_downloaded / self.total_bytes) * 100.0)


class UpdateDownloader:
    """Downloads update bundles with HTTP range-request resume."""

    def __init__(self) -> None:
        self._progress = DownloadProgress()
        self._on_progress: list[Callable[[DownloadProgress], None]] = []

    @property
    def progress(self) -> DownloadProgress:
        return self._progress

    def add_progress_callback(self, cb: Callable[[DownloadProgress], None]) -> None:
        self._on_progress.append(cb)

    def _notify(self) -> None:
        for cb in self._on_progress:
            cb(self._progress)

    async def download(self, manifest: UpdateManifest, target_dir: str) -> str:
        """Download the update bundle to target_dir, returning the final filepath.

        Uses a .tmp suffix during download, renames on completion.
        Supports resuming partial downloads via HTTP Range headers.
        """
        target_path = Path(target_dir)
        target_path.mkdir(parents=True, exist_ok=True)

        filename = f"ados-{manifest.version}.bin"
        final_file = target_path / filename
        tmp_file = target_path / (filename + ".tmp")

        existing_bytes = 0
        etag_path = target_path / (filename + ".etag")
        if tmp_file.exists():
            existing_bytes = tmp_file.stat().st_size

        self._progress = DownloadProgress(
            state=DownloadState.DOWNLOADING,
            bytes_downloaded=existing_bytes,
            total_bytes=manifest.file_size,
        )
        self._notify()

        log.info(
            "download_start",
            url=manifest.download_url,
            resume_from=existing_bytes,
            total=manifest.file_size,
        )

        headers: dict[str, str] = {}
        if existing_bytes > 0:
            headers["Range"] = f"bytes={existing_bytes}-"
            # Send If-Range so server returns 200 (full file) if content changed
            if etag_path.exists():
                saved_validator = etag_path.read_text().strip()
                if saved_validator:
                    headers["If-Range"] = saved_validator

        try:
            async with httpx.AsyncClient(timeout=300.0) as client:
                async with client.stream("GET", manifest.download_url, headers=headers) as resp:
                    resp.raise_for_status()

                    # If server returned 200 instead of 206, the file changed.
                    # Discard partial content and restart from scratch.
                    status_code = getattr(resp, "status_code", 206)
                    if existing_bytes > 0 and status_code == 200:
                        log.warning(
                            "download_resume_invalidated",
                            msg="server returned 200, restarting download",
                        )
                        existing_bytes = 0
                        self._progress.bytes_downloaded = 0

                    # Save ETag or Last-Modified for future resume validation
                    resp_headers = getattr(resp, "headers", {})
                    if isinstance(resp_headers, dict):
                        etag = resp_headers.get("ETag") or resp_headers.get("Last-Modified", "")
                    else:
                        # httpx Headers object (or similar mapping)
                        try:
                            etag = resp_headers.get("ETag") or resp_headers.get("Last-Modified", "")
                            if not isinstance(etag, str):
                                etag = ""
                        except Exception:
                            etag = ""
                    if etag:
                        etag_path.write_text(etag)

                    mode = "ab" if existing_bytes > 0 else "wb"
                    with open(tmp_file, mode) as f:
                        import time

                        last_time = time.monotonic()
                        last_bytes = existing_bytes

                        async for chunk in resp.aiter_bytes(chunk_size=65536):
                            f.write(chunk)
                            self._progress.bytes_downloaded += len(chunk)

                            now = time.monotonic()
                            elapsed = now - last_time
                            if elapsed >= 0.5:
                                delta_bytes = self._progress.bytes_downloaded - last_bytes
                                self._progress.speed_bps = delta_bytes / elapsed
                                remaining = (
                                    self._progress.total_bytes
                                    - self._progress.bytes_downloaded
                                )
                                if self._progress.speed_bps > 0:
                                    self._progress.eta_seconds = (
                                        remaining / self._progress.speed_bps
                                    )
                                last_time = now
                                last_bytes = self._progress.bytes_downloaded
                                self._notify()

        except (httpx.HTTPError, OSError) as exc:
            log.error("download_failed", error=str(exc))
            self._progress.state = DownloadState.FAILED
            self._notify()
            raise

        # Atomic rename from .tmp to final
        os.replace(str(tmp_file), str(final_file))

        # Clean up the etag file used for resume validation
        if etag_path.exists():
            etag_path.unlink(missing_ok=True)

        self._progress.state = DownloadState.COMPLETED
        self._progress.speed_bps = 0.0
        self._progress.eta_seconds = 0.0
        self._notify()

        log.info("download_complete", path=str(final_file), size=self._progress.bytes_downloaded)
        return str(final_file)
