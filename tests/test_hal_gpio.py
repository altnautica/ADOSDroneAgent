"""Tests for HAL GPIO abstraction."""

from __future__ import annotations

from unittest.mock import patch

import pytest

from ados.hal.gpio import GpioController, GpioDirection, GpioPin, detect_gpio_available


class TestDetectGpioAvailable:
    def test_non_linux_returns_false(self):
        with patch("ados.hal.gpio.platform.system", return_value="Darwin"):
            assert detect_gpio_available() is False

    def test_linux_with_sysfs(self):
        with (
            patch("ados.hal.gpio.platform.system", return_value="Linux"),
            patch("ados.hal.gpio._SYSFS_GPIO") as mock_path,
        ):
            mock_path.is_dir.return_value = True
            assert detect_gpio_available() is True

    def test_linux_without_sysfs(self):
        with (
            patch("ados.hal.gpio.platform.system", return_value="Linux"),
            patch("ados.hal.gpio._SYSFS_GPIO") as mock_path,
        ):
            mock_path.is_dir.return_value = False
            assert detect_gpio_available() is False


class TestGpioPinDataclass:
    def test_defaults(self):
        pin = GpioPin(number=17)
        assert pin.number == 17
        assert pin.direction == GpioDirection.IN
        assert pin.value == 0

    def test_output_pin(self):
        pin = GpioPin(number=27, direction=GpioDirection.OUT, value=1)
        assert pin.direction == GpioDirection.OUT
        assert pin.value == 1


class TestGpioControllerMockMode:
    """Tests for GPIO controller in mock mode (macOS / no sysfs)."""

    def setup_method(self):
        self.patcher = patch("ados.hal.gpio.detect_gpio_available", return_value=False)
        self.patcher.start()

    def teardown_method(self):
        self.patcher.stop()

    def test_mock_mode_available_false(self):
        ctrl = GpioController()
        assert ctrl.available is False

    def test_setup_pin(self):
        ctrl = GpioController()
        pin = ctrl.setup(17, GpioDirection.OUT)
        assert pin.number == 17
        assert pin.direction == GpioDirection.OUT
        assert 17 in ctrl.pins

    def test_write_in_mock_mode(self):
        ctrl = GpioController()
        ctrl.setup(17, GpioDirection.OUT)
        ctrl.write(17, 1)
        assert ctrl.pins[17].value == 1

    def test_write_clamps_to_binary(self):
        ctrl = GpioController()
        ctrl.setup(17, GpioDirection.OUT)
        ctrl.write(17, 42)
        assert ctrl.pins[17].value == 1

    def test_read_in_mock_mode(self):
        ctrl = GpioController()
        ctrl.setup(17, GpioDirection.IN)
        val = ctrl.read(17)
        assert val == 0

    def test_read_unconfigured_pin(self):
        ctrl = GpioController()
        val = ctrl.read(99)
        assert val == 0

    def test_write_unconfigured_pin(self):
        ctrl = GpioController()
        ctrl.write(99, 1)  # Should not raise

    def test_write_to_input_pin_ignored(self):
        ctrl = GpioController()
        ctrl.setup(17, GpioDirection.IN)
        ctrl.write(17, 1)
        assert ctrl.pins[17].value == 0

    def test_cleanup(self):
        ctrl = GpioController()
        ctrl.setup(17, GpioDirection.OUT)
        ctrl.setup(27, GpioDirection.IN)
        ctrl.cleanup()
        assert len(ctrl.pins) == 0


class TestGpioDirectionEnum:
    def test_values(self):
        assert GpioDirection.IN == "in"
        assert GpioDirection.OUT == "out"
