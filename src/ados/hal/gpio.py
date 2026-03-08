"""GPIO abstraction — sysfs on Linux, mock on macOS/other platforms."""

from __future__ import annotations

import platform
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path

from ados.core.logging import get_logger

log = get_logger("hal.gpio")

_SYSFS_GPIO = Path("/sys/class/gpio")


class GpioDirection(StrEnum):
    IN = "in"
    OUT = "out"


@dataclass
class GpioPin:
    """Represents a single GPIO pin's configuration and state."""

    number: int
    direction: GpioDirection = GpioDirection.IN
    value: int = 0


def detect_gpio_available() -> bool:
    """Check whether GPIO is accessible on this platform."""
    system = platform.system()
    if system == "Linux":
        return _SYSFS_GPIO.is_dir()
    log.debug("gpio_not_available", platform=system)
    return False


class GpioController:
    """Platform-aware GPIO controller.

    On Linux: drives pins through /sys/class/gpio/ sysfs interface.
    On other platforms: logs operations without touching hardware.
    """

    def __init__(self) -> None:
        self._system = platform.system()
        self._available = detect_gpio_available()
        self._pins: dict[int, GpioPin] = {}
        if not self._available:
            log.info("gpio_mock_mode", platform=self._system)

    @property
    def available(self) -> bool:
        return self._available

    @property
    def pins(self) -> dict[int, GpioPin]:
        return dict(self._pins)

    def setup(self, pin: int, direction: GpioDirection) -> GpioPin:
        """Configure a GPIO pin for input or output."""
        gpio_pin = GpioPin(number=pin, direction=direction)

        if self._available:
            self._sysfs_export(pin)
            self._sysfs_set_direction(pin, direction)

        self._pins[pin] = gpio_pin
        log.info("gpio_setup", pin=pin, direction=direction.value)
        return gpio_pin

    def read(self, pin: int) -> int:
        """Read the current value of a GPIO pin."""
        if pin not in self._pins:
            log.warning("gpio_read_unconfigured", pin=pin)
            return 0

        if self._available:
            value = self._sysfs_read(pin)
            self._pins[pin].value = value
            return value

        return self._pins[pin].value

    def write(self, pin: int, value: int) -> None:
        """Write a value (0 or 1) to a GPIO output pin."""
        if pin not in self._pins:
            log.warning("gpio_write_unconfigured", pin=pin)
            return

        clamped = 1 if value else 0

        if self._pins[pin].direction != GpioDirection.OUT:
            log.warning("gpio_write_not_output", pin=pin)
            return

        if self._available:
            self._sysfs_write(pin, clamped)

        self._pins[pin].value = clamped
        log.debug("gpio_write", pin=pin, value=clamped)

    def cleanup(self) -> None:
        """Unexport all configured GPIO pins."""
        for pin in list(self._pins):
            if self._available:
                self._sysfs_unexport(pin)
            log.debug("gpio_cleanup", pin=pin)
        self._pins.clear()
        log.info("gpio_cleanup_complete")

    # ---- sysfs helpers ----

    def _sysfs_export(self, pin: int) -> None:
        """Export a GPIO pin via sysfs."""
        pin_dir = _SYSFS_GPIO / f"gpio{pin}"
        if pin_dir.is_dir():
            return
        try:
            export_path = _SYSFS_GPIO / "export"
            export_path.write_text(str(pin))
        except OSError as exc:
            log.warning("gpio_export_failed", pin=pin, error=str(exc))

    def _sysfs_unexport(self, pin: int) -> None:
        """Unexport a GPIO pin via sysfs."""
        try:
            unexport_path = _SYSFS_GPIO / "unexport"
            unexport_path.write_text(str(pin))
        except OSError as exc:
            log.warning("gpio_unexport_failed", pin=pin, error=str(exc))

    def _sysfs_set_direction(self, pin: int, direction: GpioDirection) -> None:
        """Set the direction of a sysfs GPIO pin."""
        try:
            direction_path = _SYSFS_GPIO / f"gpio{pin}" / "direction"
            direction_path.write_text(direction.value)
        except OSError as exc:
            log.warning("gpio_direction_failed", pin=pin, error=str(exc))

    def _sysfs_read(self, pin: int) -> int:
        """Read the value of a sysfs GPIO pin."""
        try:
            value_path = _SYSFS_GPIO / f"gpio{pin}" / "value"
            raw = value_path.read_text().strip()
            return int(raw)
        except (OSError, ValueError) as exc:
            log.warning("gpio_read_failed", pin=pin, error=str(exc))
            return 0

    def _sysfs_write(self, pin: int, value: int) -> None:
        """Write a value to a sysfs GPIO pin."""
        try:
            value_path = _SYSFS_GPIO / f"gpio{pin}" / "value"
            value_path.write_text(str(value))
        except OSError as exc:
            log.warning("gpio_write_failed", pin=pin, error=str(exc))
