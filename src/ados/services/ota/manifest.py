"""Pydantic model for OTA update manifest."""

from __future__ import annotations

from pydantic import BaseModel


class UpdateManifest(BaseModel):
    """Represents an available software update from GitHub Releases."""

    version: str
    channel: str
    published_at: str
    download_url: str
    file_size: int
    sha256: str
    changelog: str
    release_url: str
