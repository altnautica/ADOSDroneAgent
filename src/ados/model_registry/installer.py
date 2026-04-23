"""Model installer — download + SHA256 verify + install.

Supports:
  - HTTP CDN download (verify SHA256)
  - Airgap local tarball install (ados models install-local <path>)

Models install to /var/ados/models/<category>/<name>/
"""

from __future__ import annotations

import hashlib
import shutil
import tarfile
import tempfile
from pathlib import Path
from typing import Any

import httpx
import structlog

from .registry import ModelRegistry, ModelManifest

log = structlog.get_logger()


class InstallError(Exception):
    pass


async def install_from_url(
    registry: ModelRegistry,
    category: str,
    name: str,
    download_url: str,
    expected_sha256: str,
    version: str = "0.0.1",
    accelerator: str = "cpu",
) -> ModelManifest:
    """Download a model from URL, verify SHA256, install, return manifest."""
    log.info("model_download_start", name=name, url=download_url)

    with tempfile.NamedTemporaryFile(suffix=".tar.gz", delete=False) as tmp:
        tmp_path = Path(tmp.name)

    try:
        async with httpx.AsyncClient(timeout=120.0) as client:
            async with client.stream("GET", download_url) as resp:
                resp.raise_for_status()
                total = int(resp.headers.get("content-length", 0))
                hasher = hashlib.sha256()
                size = 0
                with open(tmp_path, "wb") as f:
                    async for chunk in resp.aiter_bytes(8192):
                        f.write(chunk)
                        hasher.update(chunk)
                        size += len(chunk)

        actual_sha256 = hasher.hexdigest()
        if expected_sha256 and actual_sha256 != expected_sha256:
            raise InstallError(
                f"SHA256 mismatch for {name}: expected {expected_sha256}, got {actual_sha256}"
            )

        # Extract to models directory
        model_dir = registry._dir / category / name
        model_dir.mkdir(parents=True, exist_ok=True)

        if tarfile.is_tarfile(tmp_path):
            with tarfile.open(tmp_path) as tf:
                tf.extractall(model_dir)
        else:
            shutil.copy2(tmp_path, model_dir / f"{name}.bin")

        # Register in registry
        model_path = str(model_dir)
        manifest = registry.register_local(
            category=category,
            name=name,
            model_path=model_path,
            sha256=actual_sha256,
            version=version,
            accelerator=accelerator,
            size_bytes=size,
        )
        # Set download URL
        manifest.download_url = download_url
        registry._save_manifest(manifest)

        log.info("model_installed", name=name, category=category, size_bytes=size)
        return manifest

    finally:
        tmp_path.unlink(missing_ok=True)


def install_from_local_tarball(
    registry: ModelRegistry,
    category: str,
    name: str,
    tarball_path: str | Path,
    version: str = "0.0.1",
    accelerator: str = "cpu",
) -> ModelManifest:
    """Install a model from a local tarball (airgap path)."""
    path = Path(tarball_path)
    if not path.exists():
        raise InstallError(f"Tarball not found: {path}")

    # SHA256 of the tarball itself
    hasher = hashlib.sha256()
    size = 0
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(8192), b""):
            hasher.update(chunk)
            size += len(chunk)
    sha256 = hasher.hexdigest()

    model_dir = registry._dir / category / name
    model_dir.mkdir(parents=True, exist_ok=True)

    if tarfile.is_tarfile(path):
        with tarfile.open(path) as tf:
            tf.extractall(model_dir)
    else:
        shutil.copy2(path, model_dir / path.name)

    manifest = registry.register_local(
        category=category,
        name=name,
        model_path=str(model_dir),
        sha256=sha256,
        version=version,
        accelerator=accelerator,
        size_bytes=size,
    )
    log.info("model_installed_local", name=name, category=category, size_bytes=size)
    return manifest
