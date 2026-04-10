# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Vision model manager — registry fetch, model download, cache management."""

from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path
from typing import Any

import httpx

from ados.core.config import VisionConfig
from ados.core.logging import get_logger

log = get_logger("vision-models")


class DownloadState(StrEnum):
    """Model download lifecycle states."""

    IDLE = "idle"
    DOWNLOADING = "downloading"
    COMPLETED = "completed"
    FAILED = "failed"


@dataclass
class DownloadProgress:
    """Snapshot of a model download."""

    state: DownloadState = DownloadState.IDLE
    bytes_downloaded: int = 0
    total_bytes: int = 0
    speed_bps: float = 0.0
    eta_seconds: float = 0.0

    def percent(self) -> float:
        if self.total_bytes <= 0:
            return 0.0
        return min(100.0, (self.bytes_downloaded / self.total_bytes) * 100.0)

    def to_dict(self) -> dict[str, Any]:
        return {
            "state": self.state.value,
            "percent": round(self.percent(), 1),
            "bytes_downloaded": self.bytes_downloaded,
            "total_bytes": self.total_bytes,
            "speed_bps": round(self.speed_bps),
            "eta_seconds": round(self.eta_seconds),
        }


@dataclass
class ModelInfo:
    """Metadata for a single model from the registry."""

    id: str
    name: str = ""
    description: str = ""
    task: str = ""  # detection, tracking, depth, segmentation
    variants: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "task": self.task,
            "variants": self.variants,
        }


class ModelManager:
    """Fetches the model registry, manages downloads, and tracks installed models."""

    def __init__(self, config: VisionConfig, npu_tops: float = 0.0) -> None:
        self._config = config
        self._npu_tops = npu_tops
        self._models_dir = Path(config.models_dir)
        self._registry: list[ModelInfo] = []
        self._etag: str = ""
        self._etag_path = self._models_dir / ".registry_etag"
        self._cache_path = self._models_dir / ".registry_cache.json"
        self._downloads: dict[str, DownloadProgress] = {}

        # Load cached ETag
        if self._etag_path.exists():
            try:
                self._etag = self._etag_path.read_text().strip()
            except OSError:
                pass

        # Load cached registry
        if self._cache_path.exists():
            try:
                self._load_cached_registry()
            except (json.JSONDecodeError, OSError):
                pass

    def _load_cached_registry(self) -> None:
        """Load registry from local cache file."""
        data = json.loads(self._cache_path.read_text())
        models = data if isinstance(data, list) else data.get("models", [])
        self._registry = [
            ModelInfo(
                id=m.get("id", ""),
                name=m.get("name", ""),
                description=m.get("description", ""),
                task=m.get("task", ""),
                variants=m.get("variants", []),
            )
            for m in models
        ]

    async def fetch_registry(self) -> list[ModelInfo]:
        """Fetch model registry from remote URL with ETag caching."""
        url = self._config.registry_url
        if not url:
            return self._registry

        headers: dict[str, str] = {}
        if self._etag:
            headers["If-None-Match"] = self._etag

        try:
            async with httpx.AsyncClient(timeout=15.0, follow_redirects=True) as client:
                resp = await client.get(url, headers=headers)

                if resp.status_code == 304:
                    log.debug("registry_not_modified")
                    return self._registry

                resp.raise_for_status()

                # Save ETag for future requests
                new_etag = resp.headers.get("ETag", "")
                if new_etag:
                    self._etag = new_etag
                    self._models_dir.mkdir(parents=True, exist_ok=True)
                    self._etag_path.write_text(new_etag)

                data = resp.json()
                models = data if isinstance(data, list) else data.get("models", [])

                self._registry = [
                    ModelInfo(
                        id=m.get("id", ""),
                        name=m.get("name", ""),
                        description=m.get("description", ""),
                        task=m.get("task", ""),
                        variants=m.get("variants", []),
                    )
                    for m in models
                ]

                # Cache locally
                self._models_dir.mkdir(parents=True, exist_ok=True)
                self._cache_path.write_text(json.dumps(data, indent=2))

                log.info("registry_fetched", models=len(self._registry))
                return self._registry

        except (httpx.HTTPError, OSError) as exc:
            log.warning("registry_fetch_failed", error=str(exc))
            return self._registry

    def list_installed(self) -> list[dict[str, Any]]:
        """List models already installed in the models directory."""
        installed: list[dict[str, Any]] = []
        if not self._models_dir.is_dir():
            return installed

        valid_suffixes = {".rknn", ".tflite", ".onnx", ".engine"}
        for model_file in sorted(self._models_dir.iterdir()):
            if model_file.is_file() and model_file.suffix in valid_suffixes:
                installed.append({
                    "id": model_file.stem,
                    "filename": model_file.name,
                    "size_bytes": model_file.stat().st_size,
                    "format": model_file.suffix.lstrip("."),
                })
        return installed

    def select_best_variant(self, model_id: str) -> dict[str, Any] | None:
        """Select the best model variant for the current board's NPU TOPS."""
        model = None
        for m in self._registry:
            if m.id == model_id:
                model = m
                break
        if not model or not model.variants:
            return None

        # Sort variants by min_tops descending, pick the highest that fits
        eligible = [
            v for v in model.variants
            if v.get("min_tops", 0) <= self._npu_tops
        ]
        if not eligible:
            # Fall back to lowest requirement variant
            eligible = sorted(model.variants, key=lambda v: v.get("min_tops", 0))
            return eligible[0] if eligible else None

        eligible.sort(key=lambda v: v.get("min_tops", 0), reverse=True)
        return eligible[0]

    async def download_model(self, model_id: str) -> str:
        """Download a model, selecting the best variant for this board.

        Uses HTTP range-resume for interrupted downloads (same pattern as
        the OTA downloader in src/ados/services/ota/downloader.py).

        Returns the final file path on success.
        """
        variant = self.select_best_variant(model_id)
        if not variant:
            raise ValueError(f"No suitable variant found for model: {model_id}")

        download_url = variant.get("url", "")
        if not download_url:
            raise ValueError(f"No download URL for model variant: {model_id}")

        file_size = variant.get("size_bytes", 0)
        filename = variant.get("filename", f"{model_id}.rknn")

        self._models_dir.mkdir(parents=True, exist_ok=True)
        final_file = self._models_dir / filename
        tmp_file = self._models_dir / (filename + ".tmp")
        etag_file = self._models_dir / (filename + ".etag")

        existing_bytes = 0
        if tmp_file.exists():
            existing_bytes = tmp_file.stat().st_size

        progress = DownloadProgress(
            state=DownloadState.DOWNLOADING,
            bytes_downloaded=existing_bytes,
            total_bytes=file_size,
        )
        self._downloads[model_id] = progress

        log.info(
            "model_download_start",
            model=model_id,
            url=download_url,
            resume_from=existing_bytes,
            total=file_size,
        )

        headers: dict[str, str] = {}
        if existing_bytes > 0:
            headers["Range"] = f"bytes={existing_bytes}-"
            if etag_file.exists():
                saved_validator = etag_file.read_text().strip()
                if saved_validator:
                    headers["If-Range"] = saved_validator

        try:
            async with httpx.AsyncClient(timeout=300.0, follow_redirects=True) as client:
                async with client.stream("GET", download_url, headers=headers) as resp:
                    resp.raise_for_status()

                    status_code = getattr(resp, "status_code", 206)
                    if existing_bytes > 0 and status_code == 200:
                        log.warning(
                            "model_download_resume_invalidated",
                            msg="server returned 200, restarting download",
                        )
                        existing_bytes = 0
                        progress.bytes_downloaded = 0

                    # Save ETag for resume validation
                    resp_etag = resp.headers.get("ETag") or resp.headers.get("Last-Modified", "")
                    if resp_etag:
                        etag_file.write_text(resp_etag)

                    mode = "ab" if existing_bytes > 0 else "wb"
                    with open(tmp_file, mode) as f:
                        last_time = time.monotonic()
                        last_bytes = existing_bytes

                        async for chunk in resp.aiter_bytes(chunk_size=65536):
                            f.write(chunk)
                            progress.bytes_downloaded += len(chunk)

                            now = time.monotonic()
                            elapsed = now - last_time
                            if elapsed >= 0.5:
                                delta_bytes = progress.bytes_downloaded - last_bytes
                                progress.speed_bps = delta_bytes / elapsed
                                remaining = progress.total_bytes - progress.bytes_downloaded
                                if progress.speed_bps > 0:
                                    progress.eta_seconds = remaining / progress.speed_bps
                                last_time = now
                                last_bytes = progress.bytes_downloaded

        except (httpx.HTTPError, OSError) as exc:
            log.error("model_download_failed", model=model_id, error=str(exc))
            progress.state = DownloadState.FAILED
            raise

        # Atomic rename
        os.replace(str(tmp_file), str(final_file))

        if etag_file.exists():
            etag_file.unlink(missing_ok=True)

        progress.state = DownloadState.COMPLETED
        progress.speed_bps = 0.0
        progress.eta_seconds = 0.0

        log.info("model_download_complete", model=model_id, path=str(final_file))
        return str(final_file)

    def get_download_progress(self, model_id: str) -> DownloadProgress:
        """Get the current download progress for a model."""
        return self._downloads.get(model_id, DownloadProgress())

    def get_cache_usage(self) -> dict[str, Any]:
        """Report total model cache size and limit."""
        total_bytes = 0
        if self._models_dir.is_dir():
            valid_suffixes = {".rknn", ".tflite", ".onnx", ".engine"}
            for f in self._models_dir.iterdir():
                if f.is_file() and f.suffix in valid_suffixes:
                    total_bytes += f.stat().st_size

        max_bytes = self._config.models_cache_max_mb * 1024 * 1024
        return {
            "used_bytes": total_bytes,
            "max_bytes": max_bytes,
            "used_mb": round(total_bytes / (1024 * 1024), 1),
            "max_mb": self._config.models_cache_max_mb,
        }

    @property
    def registry(self) -> list[ModelInfo]:
        return list(self._registry)
