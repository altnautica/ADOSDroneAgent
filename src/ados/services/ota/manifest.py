"""Pydantic model for OTA update manifest."""

from __future__ import annotations

from pydantic import BaseModel


class UpdateManifest(BaseModel):
    """Represents an available firmware/software update."""

    version: str
    channel: str
    release_date: str
    download_url: str
    file_size: int
    sha256: str
    signature: str
    min_version: str
    changelog: str
    requires_reboot: bool
