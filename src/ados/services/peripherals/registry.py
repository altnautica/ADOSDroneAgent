"""Runtime registry for peripheral manifests.

Keeps the merged set of manifests in memory, exposes lookup and
listing helpers, and provides a ``reload()`` hook driven by SIGHUP.
Wave 3 stubs out live transport detection: every peripheral reports
``connected: False``. Track B fills in per-transport probing (lsusb
scans, serial enumeration, mDNS lookups, BLE discovery) behind the
same API.
"""

from __future__ import annotations

import threading

from ados.core.logging import get_logger
from ados.core.paths import PERIPHERALS_GLOB
from ados.services.peripherals.loader import load_all
from ados.services.peripherals.manifest import PeripheralManifest

log = get_logger("peripherals.registry")


class PeripheralRegistry:
    """Process-wide peripheral manifest registry.

    Thread-safe for the read and reload paths. The filesystem glob is
    configurable to keep unit tests honest.
    """

    def __init__(
        self,
        glob_path: str = PERIPHERALS_GLOB,
    ) -> None:
        self._glob_path = glob_path
        self._lock = threading.Lock()
        self._by_id: dict[str, PeripheralManifest] = {}
        self._order: list[str] = []
        self.reload()

    def reload(self) -> int:
        """Re-read manifests from entry points and the filesystem.

        Returns the number of manifests registered after the reload.
        Safe to call concurrently with list()/get().
        """
        manifests = load_all(self._glob_path)
        with self._lock:
            self._by_id = {m.id: m for m in manifests}
            self._order = [m.id for m in manifests]
        log.info("peripheral_registry_reloaded", count=len(manifests))
        return len(manifests)

    def _detect_connection(self, manifest: PeripheralManifest) -> bool:
        """Wave 3 stub. Track B replaces this with real transport probing.

        The signature stays stable so callers can rely on it: returns
        True if the manifest currently matches a live device on its
        declared transport, False otherwise. Wave 3 always returns
        False because no built-in plugins ship and we do not want to
        claim devices the plugin layer has not validated yet.
        """
        return False

    def _envelope(self, manifest: PeripheralManifest) -> dict:
        """Return the public-facing manifest dict plus live status."""
        data = manifest.model_dump()
        status_endpoint = data.get("status_endpoint")
        if not status_endpoint:
            data["status_endpoint"] = f"/api/v1/peripherals/{manifest.id}"
        data["connected"] = self._detect_connection(manifest)
        return data

    def list(self) -> list[dict]:
        """Return every registered manifest with live connection status."""
        with self._lock:
            ordered = [self._by_id[pid] for pid in self._order if pid in self._by_id]
        return [self._envelope(m) for m in ordered]

    def get(self, peripheral_id: str) -> dict | None:
        """Return a single manifest plus status, or None if unregistered."""
        with self._lock:
            manifest = self._by_id.get(peripheral_id)
        if manifest is None:
            return None
        return self._envelope(manifest)

    def has(self, peripheral_id: str) -> bool:
        """Return True if the given id is registered."""
        with self._lock:
            return peripheral_id in self._by_id

    def get_manifest(self, peripheral_id: str) -> PeripheralManifest | None:
        """Return the raw manifest object for internal consumers."""
        with self._lock:
            return self._by_id.get(peripheral_id)


# ----------------------------------------------------------------------
# Module-level singleton
# ----------------------------------------------------------------------
# Same pattern as get_input_manager() / get_pic_arbiter() /
# get_pair_manager(). Single instance per agent process; test code can
# call _reset_for_tests() to drop the cache.
_instance: PeripheralRegistry | None = None
_instance_lock = threading.Lock()


def get_peripheral_registry() -> PeripheralRegistry:
    """Return the process-wide PeripheralRegistry singleton."""
    global _instance
    if _instance is None:
        with _instance_lock:
            if _instance is None:
                _instance = PeripheralRegistry()
    return _instance


def _reset_for_tests() -> None:
    """Drop the cached singleton. Test-only helper."""
    global _instance
    with _instance_lock:
        _instance = None
