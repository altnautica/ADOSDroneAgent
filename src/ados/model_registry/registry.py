"""Model registry — on-demand install, ref counting, LRU eviction.

Models install to /var/ados/models/<category>/<name>/
Each model directory has a manifest.json with sha256, size, accelerator.

Install flow:
  1. Fetch manifest from CDN (or local tarball for airgap)
  2. SHA256-verify the download
  3. Install to the models directory
  4. Increment ref count
  5. Update manifest.json

Eviction:
  - LRU order (least recently used first)
  - Only evict zero-ref models
  - Stop evicting when under disk pressure threshold
  - Pinned models never evict
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import time
from pathlib import Path
from typing import Any

import structlog

log = structlog.get_logger()

MODELS_DIR = Path(os.environ.get("ADOS_MODELS_DIR", "/var/ados/models"))

# Disk budget per tier (bytes)
TIER_DISK_BUDGET = {
    1: 500 * 1024 * 1024,          # 500 MB
    2: 2 * 1024 * 1024 * 1024,     # 2 GB
    3: 10 * 1024 * 1024 * 1024,    # 10 GB
    4: 50 * 1024 * 1024 * 1024,    # 50 GB
}

CATEGORIES = {
    "detect", "tracker", "depth", "embed", "caption",
    "narrate", "tts", "asr", "translate", "anomaly", "ocr",
}

# Accelerator priority: highest first
ACCELERATOR_PRIORITY = ["rknn", "tflite", "onnx", "cpu"]


class ModelManifest:
    """Parsed model manifest from /var/ados/models/<cat>/<name>/manifest.json."""

    def __init__(self, data: dict[str, Any]) -> None:
        self.name: str = data["name"]
        self.category: str = data["category"]
        self.version: str = data.get("version", "0.0.1")
        self.sha256: str = data.get("sha256", "")
        self.size_bytes: int = data.get("size_bytes", 0)
        self.accelerator: str = data.get("accelerator", "cpu")
        self.ref_count: int = data.get("ref_count", 0)
        self.pinned: bool = data.get("pinned", False)
        self.installed_at: float = data.get("installed_at", 0.0)
        self.last_used_at: float = data.get("last_used_at", 0.0)
        self.model_path: str = data.get("model_path", "")
        self.download_url: str = data.get("download_url", "")

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name, "category": self.category,
            "version": self.version, "sha256": self.sha256,
            "size_bytes": self.size_bytes, "accelerator": self.accelerator,
            "ref_count": self.ref_count, "pinned": self.pinned,
            "installed_at": self.installed_at, "last_used_at": self.last_used_at,
            "model_path": self.model_path, "download_url": self.download_url,
        }


class ModelRegistry:
    """Manages on-drone ML models."""

    def __init__(self, models_dir: Path = MODELS_DIR) -> None:
        self._dir = models_dir
        self._dir.mkdir(parents=True, exist_ok=True)

    def _manifest_path(self, category: str, name: str) -> Path:
        return self._dir / category / name / "manifest.json"

    def _load_manifest(self, category: str, name: str) -> ModelManifest | None:
        path = self._manifest_path(category, name)
        if not path.exists():
            return None
        try:
            return ModelManifest(json.loads(path.read_text()))
        except Exception:
            return None

    def _save_manifest(self, manifest: ModelManifest) -> None:
        path = self._manifest_path(manifest.category, manifest.name)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(manifest.to_dict(), indent=2))

    def list_installed(self) -> list[ModelManifest]:
        results = []
        for cat_dir in self._dir.iterdir():
            if not cat_dir.is_dir():
                continue
            for model_dir in cat_dir.iterdir():
                if not model_dir.is_dir():
                    continue
                m = self._load_manifest(cat_dir.name, model_dir.name)
                if m:
                    results.append(m)
        return results

    def get(self, category: str, name: str) -> ModelManifest | None:
        return self._load_manifest(category, name)

    def is_installed(self, category: str, name: str) -> bool:
        return self._manifest_path(category, name).exists()

    def add_ref(self, category: str, name: str) -> bool:
        m = self._load_manifest(category, name)
        if not m:
            return False
        m.ref_count += 1
        m.last_used_at = time.time()
        self._save_manifest(m)
        return True

    def remove_ref(self, category: str, name: str) -> bool:
        m = self._load_manifest(category, name)
        if not m:
            return False
        m.ref_count = max(0, m.ref_count - 1)
        self._save_manifest(m)
        return True

    def pin(self, category: str, name: str) -> bool:
        m = self._load_manifest(category, name)
        if not m:
            return False
        m.pinned = True
        self._save_manifest(m)
        return True

    def register_local(
        self,
        category: str,
        name: str,
        model_path: str,
        sha256: str = "",
        version: str = "0.0.1",
        accelerator: str = "cpu",
        size_bytes: int = 0,
    ) -> ModelManifest:
        """Register a locally-present model (airgap path)."""
        m = ModelManifest({
            "name": name, "category": category, "version": version,
            "sha256": sha256, "size_bytes": size_bytes,
            "accelerator": accelerator, "ref_count": 0, "pinned": False,
            "installed_at": time.time(), "last_used_at": time.time(),
            "model_path": model_path, "download_url": "",
        })
        self._save_manifest(m)
        log.info("model_registered_local", name=name, category=category, path=model_path)
        return m

    def evict_lru(self, target_free_bytes: int) -> int:
        """Evict LRU zero-ref non-pinned models to free space. Returns bytes freed."""
        models = [m for m in self.list_installed() if not m.pinned and m.ref_count == 0]
        models.sort(key=lambda m: m.last_used_at)
        freed = 0
        for m in models:
            model_dir = self._dir / m.category / m.name
            size = sum(f.stat().st_size for f in model_dir.rglob("*") if f.is_file())
            shutil.rmtree(model_dir, ignore_errors=True)
            freed += size
            log.info("model_evicted_lru", name=m.name, category=m.category, bytes_freed=size)
            if freed >= target_free_bytes:
                break
        return freed

    def total_disk_usage(self) -> int:
        return sum(
            f.stat().st_size
            for f in self._dir.rglob("*")
            if f.is_file()
        )


# Module-level singleton
_registry: ModelRegistry | None = None


def get_registry() -> ModelRegistry:
    global _registry
    if _registry is None:
        _registry = ModelRegistry()
    return _registry
