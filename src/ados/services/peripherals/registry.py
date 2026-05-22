"""Runtime registry for peripheral manifests.

Keeps the merged set of manifests in memory, exposes lookup and
listing helpers, and provides a ``reload()`` hook driven by SIGHUP.
Connection detection is best-effort: ``regex``-style matches walk
``/dev/*`` for a path match; ``vid``/``pid`` matches walk the live
sysfs USB tree. Probes are short and cached lightly by the caller
through normal API polling.
"""

from __future__ import annotations

import glob as _glob
import re
import threading
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import PERIPHERALS_GLOB
from ados.services.peripherals.loader import load_all
from ados.services.peripherals.manifest import PeripheralManifest

log = get_logger("peripherals.registry")

# Where USB devices advertise vid/pid in sysfs. One file per device,
# four-digit lowercase hex without prefix.
_USB_DEVICES_ROOT = Path("/sys/bus/usb/devices")


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
        """Return whether the manifest matches a currently detected device.

        Three match modes, evaluated in order:
        - regex: walks ``/dev`` for any path matching the manifest's
          regex via ``re.fullmatch``. Anchors in the manifest become
          functionally redundant since fullmatch is implicitly
          anchored, but they're harmless and improve readability.
        - vid: walks ``/sys/bus/usb/devices/*/idVendor`` for any
          device whose vid matches.
        - pid: same, against ``idProduct``. Combined with vid when both
          are declared.
        All probes are best-effort and silent on error: an inaccessible
        sysfs path on a non-Linux dev host returns False rather than
        raising.
        """
        match = manifest.match
        if match.regex:
            try:
                pattern = re.compile(match.regex)
            except re.error:
                log.warning(
                    "peripheral_regex_invalid",
                    peripheral_id=manifest.id,
                    regex=match.regex,
                )
                return False
            for candidate in _glob.iglob("/dev/**/*", recursive=True):
                if pattern.fullmatch(candidate):
                    return True
            return False

        if match.vid or match.pid:
            want_vid = (match.vid or "").lower().removeprefix("0x")
            want_pid = (match.pid or "").lower().removeprefix("0x")
            if not _USB_DEVICES_ROOT.is_dir():
                return False
            for dev in _USB_DEVICES_ROOT.iterdir():
                try:
                    if want_vid:
                        vid = (dev / "idVendor").read_text().strip().lower()
                        if vid != want_vid:
                            continue
                    if want_pid:
                        pid = (dev / "idProduct").read_text().strip().lower()
                        if pid != want_pid:
                            continue
                    return True
                except OSError:
                    continue
            return False

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
