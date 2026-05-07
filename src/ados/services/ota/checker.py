"""Update checker using GitHub Releases API."""

from __future__ import annotations

import asyncio
import re
from collections.abc import Callable

import httpx

from ados.core.config import OtaConfig
from ados.core.logging import get_logger
from ados.services.ota.manifest import UpdateManifest

log = get_logger("ota-checker")

GITHUB_API = "https://api.github.com"

# Full-agent release tags only. Other tag lines (e.g. lite agent at
# `lite-vX.Y.Z`, image builds at `lite-image-vX.Y.Z`) ship from the same
# repo and must not be considered as upgrade candidates for the full agent.
_FULL_AGENT_TAG = re.compile(r"^v\d+\.\d+\.\d+$")


def _version_tuple(version: str) -> tuple[int, ...] | None:
    """Parse a strict semver string into a comparable tuple.

    Returns None if any segment is not a non-negative integer. Callers must
    treat None as a hard failure rather than silently comparing 0s.
    """
    parts: list[int] = []
    for segment in version.lstrip("v").split("."):
        try:
            parts.append(int(segment))
        except ValueError:
            return None
    return tuple(parts)


class UpdateChecker:
    """Checks GitHub Releases for new versions."""

    def __init__(
        self,
        config: OtaConfig,
        on_update_found: Callable[[UpdateManifest], None] | None = None,
    ) -> None:
        self._config = config
        self._on_update_found = on_update_found
        self._last_manifest: UpdateManifest | None = None
        self._etag: str = ""
        self._cached_manifest: UpdateManifest | None = None

    @property
    def last_manifest(self) -> UpdateManifest | None:
        return self._last_manifest

    async def check_for_update(self, current_version: str) -> UpdateManifest | None:
        """Fetch the latest release from GitHub and compare versions.

        Returns the manifest if a newer version is available, None otherwise.
        """
        repo = self._config.github_repo
        channel = self._config.channel

        # Always list releases. The /releases/latest endpoint returns the
        # newest non-prerelease across every release line in the repo,
        # which is wrong as soon as more than one tag scheme ships from
        # the same repo (full agent + lite agent + image builds).
        url = f"{GITHUB_API}/repos/{repo}/releases"

        log.info("checking_for_update", url=url, current=current_version, channel=channel)

        headers: dict[str, str] = {"Accept": "application/vnd.github+json"}
        if self._etag:
            headers["If-None-Match"] = self._etag

        try:
            async with httpx.AsyncClient(timeout=30.0, follow_redirects=True) as client:
                resp = await client.get(url, headers=headers)

                # ETag cache hit: no new data
                if resp.status_code == 304:
                    log.info("github_cache_hit", msg="no changes since last check")
                    if self._cached_manifest:
                        cached_tuple = _version_tuple(self._cached_manifest.version)
                        current_tuple = _version_tuple(current_version)
                        if cached_tuple is None or current_tuple is None:
                            log.warning(
                                "version_parse_failed_on_cache_hit",
                                cached=self._cached_manifest.version,
                                current=current_version,
                            )
                            return None
                        if cached_tuple > current_tuple:
                            return self._cached_manifest
                        return None
                    return None

                if resp.status_code == 403:
                    log.warning("github_rate_limited", msg="rate limit hit, skipping check")
                    return None

                resp.raise_for_status()

                # Save ETag for future requests
                new_etag = resp.headers.get("ETag", "")
                if new_etag:
                    self._etag = new_etag

                data = resp.json()

        except httpx.HTTPError as exc:
            log.warning("update_check_failed", error=str(exc))
            return None

        if not isinstance(data, list):
            log.warning("releases_response_not_list", type=type(data).__name__)
            return None

        # Walk newest-first (GitHub orders by created_at desc) and pick the
        # first non-draft release that matches the full-agent tag pattern.
        # Stable channel additionally rejects prereleases. Lite agent and
        # image-build releases ship from this same repo with their own tag
        # prefixes and must be skipped here.
        release = None
        for r in data:
            if r.get("draft", False):
                continue
            if channel == "stable" and r.get("prerelease", False):
                continue
            tag = r.get("tag_name", "")
            if not _FULL_AGENT_TAG.match(tag):
                continue
            release = r
            break

        if release is None:
            log.info("no_eligible_release", channel=channel, candidates=len(data))
            return None

        release_version = release.get("tag_name", "").lstrip("v")
        if not release_version:
            log.warning("release_missing_tag")
            return None

        release_tuple = _version_tuple(release_version)
        current_tuple = _version_tuple(current_version)
        if release_tuple is None or current_tuple is None:
            log.warning(
                "version_parse_failed",
                latest=release_version,
                current=current_version,
            )
            return None
        if release_tuple <= current_tuple:
            log.info("no_update_available", latest=release_version, current=current_version)
            return None

        # Find wheel asset and SHA256SUMS
        assets = release.get("assets", [])
        wheel_asset = None
        sha256_asset = None

        for asset in assets:
            name = asset.get("name", "")
            if name.endswith(".whl"):
                wheel_asset = asset
            elif name == "SHA256SUMS":
                sha256_asset = asset

        if not wheel_asset:
            log.warning("no_wheel_asset", version=release_version)
            return None

        # Get SHA256 digest
        sha256_hex = ""
        if sha256_asset:
            sha256_hex = await self._fetch_sha256(sha256_asset, wheel_asset["name"])

        if not sha256_hex:
            log.warning(
                "no_sha256_available",
                version=release_version,
                msg="update will skip hash verification",
            )

        manifest = UpdateManifest(
            version=release_version,
            channel=channel,
            published_at=release.get("published_at", ""),
            download_url=wheel_asset["browser_download_url"],
            file_size=wheel_asset.get("size", 0),
            sha256=sha256_hex,
            changelog=release.get("body", "") or "",
            release_url=release.get("html_url", ""),
        )

        self._last_manifest = manifest
        self._cached_manifest = manifest

        log.info("update_available", version=manifest.version)

        if self._on_update_found:
            self._on_update_found(manifest)

        return manifest

    async def _fetch_sha256(self, sha256_asset: dict, wheel_name: str) -> str:
        """Download SHA256SUMS and extract the digest for the wheel file."""
        url = sha256_asset.get("browser_download_url", "")
        if not url:
            return ""

        try:
            async with httpx.AsyncClient(timeout=15.0, follow_redirects=True) as client:
                resp = await client.get(url)
                resp.raise_for_status()
                text = resp.text

            for line in text.strip().splitlines():
                parts = line.strip().split()
                if len(parts) >= 2 and parts[1].lstrip("*") == wheel_name:
                    return parts[0]

            log.warning("sha256_wheel_not_in_sums", wheel=wheel_name)
            return ""

        except httpx.HTTPError as exc:
            log.warning("sha256_fetch_failed", error=str(exc))
            return ""

    async def run(self, current_version: str) -> None:
        """Periodically check for updates."""
        interval = self._config.check_interval * 3600
        log.info("checker_started", interval_hours=self._config.check_interval)

        while True:
            await self.check_for_update(current_version)
            await asyncio.sleep(interval)
