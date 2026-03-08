"""Periodic update checker against the ADOS update server."""

from __future__ import annotations

import asyncio
from collections.abc import Callable

import httpx

from ados.core.config import OtaConfig
from ados.core.logging import get_logger
from ados.services.ota.manifest import UpdateManifest

log = get_logger("ota-checker")


def _version_tuple(version: str) -> tuple[int, ...]:
    """Parse a semver string into a comparable tuple."""
    parts: list[int] = []
    for segment in version.lstrip("v").split("."):
        try:
            parts.append(int(segment))
        except ValueError:
            parts.append(0)
    return tuple(parts)


class UpdateChecker:
    """Checks the update server for new versions."""

    def __init__(
        self,
        config: OtaConfig,
        on_update_found: Callable[[UpdateManifest], None] | None = None,
    ) -> None:
        self._config = config
        self._on_update_found = on_update_found
        self._last_manifest: UpdateManifest | None = None

    @property
    def last_manifest(self) -> UpdateManifest | None:
        return self._last_manifest

    async def check_for_update(self, current_version: str) -> UpdateManifest | None:
        """Fetch the latest manifest and compare versions.

        Returns the manifest if a newer version is available, None otherwise.
        """
        url = (
            f"{self._config.server}/api/v1/updates"
            f"/{self._config.channel}/latest.json"
        )
        log.info("checking_for_update", url=url, current=current_version)

        try:
            async with httpx.AsyncClient(timeout=30.0) as client:
                resp = await client.get(url)
                resp.raise_for_status()
                data = resp.json()
        except httpx.HTTPError as exc:
            log.warning("update_check_failed", error=str(exc))
            return None

        manifest = UpdateManifest(**data)

        if _version_tuple(manifest.version) <= _version_tuple(current_version):
            log.info("no_update_available", latest=manifest.version, current=current_version)
            return None

        if _version_tuple(current_version) < _version_tuple(manifest.min_version):
            log.warning(
                "update_requires_newer_base",
                current=current_version,
                min_required=manifest.min_version,
            )
            return None

        log.info("update_available", version=manifest.version)
        self._last_manifest = manifest

        if self._on_update_found:
            self._on_update_found(manifest)

        return manifest

    async def run(self, current_version: str) -> None:
        """Periodically check for updates."""
        interval = self._config.check_interval * 3600
        log.info("checker_started", interval_hours=self._config.check_interval)

        while True:
            await self.check_for_update(current_version)
            await asyncio.sleep(interval)
