"""Tests for HAL USB hotplug monitor."""

from __future__ import annotations

import asyncio
from unittest.mock import patch

import pytest

from ados.hal.hotplug import HotplugMonitor, _device_key
from ados.hal.usb import UsbCategory, UsbDevice


def _make_device(
    vid: int = 0x0483,
    pid: int = 0x5740,
    name: str = "STM32",
    bus: str = "001",
    device: str = "003",
) -> UsbDevice:
    return UsbDevice(
        vid=vid,
        pid=pid,
        name=name,
        bus=bus,
        device=device,
        description="Test Device",
        category=UsbCategory.FC,
    )


class TestDeviceKey:
    def test_unique_key(self):
        dev = _make_device()
        key = _device_key(dev)
        assert "0483" in key
        assert "5740" in key

    def test_different_devices_different_keys(self):
        dev1 = _make_device(vid=0x0483, pid=0x5740)
        dev2 = _make_device(vid=0x0BDA, pid=0x8812)
        assert _device_key(dev1) != _device_key(dev2)


class TestHotplugMonitor:
    def test_initial_state(self):
        monitor = HotplugMonitor()
        assert monitor.running is False
        assert monitor.known_devices == {}

    def test_scan_diff_detects_additions(self):
        dev1 = _make_device()
        monitor = HotplugMonitor()
        events: list[tuple[str, UsbDevice]] = []

        with patch("ados.hal.hotplug.discover_usb_devices", return_value=[dev1]):
            monitor._scan_diff(lambda event, device: events.append((event, device)))

        assert len(events) == 1
        assert events[0][0] == "add"
        assert events[0][1].vid == 0x0483

    def test_scan_diff_detects_removals(self):
        dev1 = _make_device()
        monitor = HotplugMonitor()
        events: list[tuple[str, UsbDevice]] = []

        # First scan: add device
        with patch("ados.hal.hotplug.discover_usb_devices", return_value=[dev1]):
            monitor._scan_diff(lambda e, d: None)

        # Second scan: device gone
        with patch("ados.hal.hotplug.discover_usb_devices", return_value=[]):
            monitor._scan_diff(lambda event, device: events.append((event, device)))

        assert len(events) == 1
        assert events[0][0] == "remove"

    def test_scan_diff_no_change(self):
        dev1 = _make_device()
        monitor = HotplugMonitor()
        events: list[tuple[str, UsbDevice]] = []

        with patch("ados.hal.hotplug.discover_usb_devices", return_value=[dev1]):
            monitor._scan_diff(lambda e, d: None)

        # Same device still present
        with patch("ados.hal.hotplug.discover_usb_devices", return_value=[dev1]):
            monitor._scan_diff(lambda event, device: events.append((event, device)))

        assert len(events) == 0

    def test_stop(self):
        monitor = HotplugMonitor()
        monitor._running = True
        monitor.stop()
        assert monitor.running is False

    @pytest.mark.asyncio
    async def test_run_can_be_stopped(self):
        monitor = HotplugMonitor()
        events: list[tuple[str, UsbDevice]] = []

        with patch("ados.hal.hotplug.discover_usb_devices", return_value=[]):
            task = asyncio.create_task(
                monitor.run(lambda e, d: events.append((e, d)))
            )
            await asyncio.sleep(0.1)
            monitor.stop()
            # Give the loop time to see the stop flag
            await asyncio.sleep(0.2)
            if not task.done():
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass
