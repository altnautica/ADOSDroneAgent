"""USB hot-plug monitoring. Polls for device add/remove events."""

from __future__ import annotations

import asyncio
import platform
from collections.abc import Callable
from typing import Any

from ados.core.logging import get_logger
from ados.hal.usb import UsbDevice, discover_usb_devices

log = get_logger("hal.hotplug")

# Callback type: (event: str, device: UsbDevice) -> None
HotplugCallback = Callable[[str, UsbDevice], Any]

# Poll interval: Linux checks faster because sysfs is cheap.
_LINUX_POLL_INTERVAL = 2.0
_MACOS_POLL_INTERVAL = 5.0


def _device_key(dev: UsbDevice) -> str:
    """Generate a stable unique key for a USB device.

    `dev.device` is the USB bus device number, which the kernel changes
    on every re-enumeration (e.g. unplug + replug or a DFU/flight-mode
    transition on SpeedyBee boards). Including it in the key caused
    spurious "add" events for the same physical device on every re-enum.
    Use bus + name + vid/pid instead, which is stable enough to identify
    the same physical device across re-enums.
    """
    return f"{dev.vid:04x}:{dev.pid:04x}:{dev.bus}:{dev.name}"


class HotplugMonitor:
    """Watches for USB device add/remove events by periodic polling.

    Linux: polls every 2 seconds.
    macOS: polls every 5 seconds (system_profiler is slower).
    """

    def __init__(self) -> None:
        self._system = platform.system()
        self._known: dict[str, UsbDevice] = {}
        self._running = False
        if self._system == "Linux":
            self._interval = _LINUX_POLL_INTERVAL
        else:
            self._interval = _MACOS_POLL_INTERVAL

    @property
    def running(self) -> bool:
        return self._running

    @property
    def poll_interval(self) -> float:
        """Effective poll interval in seconds for the current platform."""
        return self._interval

    @property
    def known_devices(self) -> dict[str, UsbDevice]:
        return dict(self._known)

    def _scan_diff(
        self,
        callback: HotplugCallback,
    ) -> None:
        """Perform one scan cycle and fire callbacks for changes."""
        current_devices = discover_usb_devices()
        current_map: dict[str, UsbDevice] = {}
        for dev in current_devices:
            key = _device_key(dev)
            current_map[key] = dev

        # Detect additions
        for key, dev in current_map.items():
            if key not in self._known:
                log.info("usb_device_added", name=dev.name, category=dev.category.value)
                callback("add", dev)

        # Detect removals
        for key, dev in self._known.items():
            if key not in current_map:
                log.info("usb_device_removed", name=dev.name, category=dev.category.value)
                callback("remove", dev)

        self._known = current_map

    async def run(self, callback: HotplugCallback) -> None:
        """Poll for USB device changes until cancelled.

        On first run, all existing devices are reported as "add" events.
        """
        self._running = True
        log.info(
            "hotplug_monitor_start",
            platform=self._system,
            interval=self._interval,
        )

        try:
            while self._running:
                try:
                    loop = asyncio.get_running_loop()
                    await loop.run_in_executor(None, self._scan_diff, callback)
                except Exception as exc:
                    log.warning("hotplug_scan_error", error=str(exc))
                await asyncio.sleep(self._interval)
        finally:
            self._running = False
            log.info("hotplug_monitor_stop")

    def stop(self) -> None:
        """Signal the monitor to stop on the next iteration."""
        self._running = False
