"""ROCK 5B+ and ROCK 5C board profiles: detection and sensor bindings.

Detection runs the real pipeline over the device-tree strings the Radxa vendor
kernel reports (``rk3588-rock-5b-plus.dts``, ``rk3588-rock-5b.dts``,
``rk3588s-rock-5c.dts``); the bindings are asserted on the validated profile,
the same object the services read.
"""

from __future__ import annotations

from unittest.mock import patch

import pytest

from ados.hal.detect import (
    BoardProfile,
    _load_board_profiles,
    detect_board,
    detect_board_profile,
    invalidate_board_info_cache,
)


@pytest.fixture(autouse=True)
def _clear_board_cache():
    invalidate_board_info_cache()
    yield
    invalidate_board_info_cache()


def _resolve(compatible: str, model: str, cpu_cores: int = 8):
    """Run detection over injected host facts; return (BoardInfo, BoardProfile)."""
    with (
        patch("ados.hal.detect._read_board_override", return_value=""),
        patch("ados.hal.detect._read_device_compatible", return_value=compatible),
        patch("ados.hal.detect._read_device_model", return_value=model),
        patch("ados.hal.detect._read_cpuinfo_model", return_value=""),
        patch("ados.hal.detect.platform.system", return_value="Linux"),
        patch("ados.hal.detect.platform.machine", return_value="aarch64"),
        patch("psutil.virtual_memory") as mock_mem,
        patch("psutil.cpu_count", return_value=cpu_cores),
    ):
        mock_mem.return_value = type("VMem", (), {"total": 16 * 1024**3})()
        return detect_board(force=True), detect_board_profile(force=True)


def _profile(stem: str) -> BoardProfile:
    return next(p for p in _load_board_profiles() if p.stem == stem)


class TestDetection:
    @pytest.mark.parametrize(
        "compatible,model",
        [
            ("radxa,rock-5b-plus", "Radxa ROCK 5B+"),
            # Compatible node unreadable: the model string alone still resolves.
            ("", "Radxa ROCK 5B+"),
        ],
    )
    def test_rock_5b_plus_resolves_to_its_profile(self, compatible: str, model: str):
        board, profile = _resolve(compatible, model)
        assert board.profile == "rock-5b-plus"
        assert board.name == "Radxa ROCK 5B+ (RK3588)"
        assert board.soc == "RK3588"
        assert board.has_accelerator
        assert profile is not None and profile.stem == "rock-5b-plus"

    @pytest.mark.parametrize(
        "compatible,model",
        [("radxa,rock-5b", "Radxa ROCK 5B"), ("", "Radxa ROCK 5B")],
    )
    def test_plain_rock_5b_is_not_the_5b_plus(self, compatible: str, model: str):
        """The token rock-5b is a prefix of rock-5b-plus, never the reverse."""
        board, _profile_ = _resolve(compatible, model)
        assert board.profile != "rock-5b-plus"
        assert board.profile == "generic-arm64"

    @pytest.mark.parametrize(
        "cpu_cores,name,soc",
        [
            (8, "Radxa ROCK 5C (RK3588S2)", "RK3588S2"),
            (6, "Radxa ROCK 5C Lite (RK3582)", "RK3582"),
        ],
    )
    def test_rock_5c_resolves_to_the_shared_profile(
        self, cpu_cores: int, name: str, soc: str
    ):
        board, profile = _resolve("radxa,rock-5c", "Radxa ROCK 5C", cpu_cores)
        assert board.profile == "rock-5c-lite"
        assert board.name == name
        assert board.soc == soc
        # The variant renames the part; the sensor bindings are the PCB's.
        assert profile is not None
        assert profile.gnss is not None and profile.gnss.pps_gpio == 37
        assert profile.buttons is not None
        assert profile.battery_gauge is None


class TestRock5BPlusBindings:
    def test_both_csi_connectors_are_declared(self):
        cams = {c.id: c for c in _profile("rock-5b-plus").cameras.supported}
        assert set(cams) == {"csi-cam0", "csi-cam1"}
        assert cams["csi-cam0"].bus == "csi2_dphy0"
        assert cams["csi-cam1"].bus == "csi2_dphy1"
        for cam in cams.values():
            assert cam.type == "csi"
            assert cam.overlay_required
            assert cam.overlay_source == "bsp-disabled"
            assert cam.overlay_ref is not None
            assert cam.overlay_ref.startswith("rock-5b-plus-cam")
        assert _profile("rock-5b-plus").video.csi_ports == 2

    def test_gnss_is_the_header_uart_with_a_pps_line(self):
        gnss = _profile("rock-5b-plus").gnss
        assert gnss is not None
        assert gnss.transport == "serial"
        assert gnss.device == "/dev/ttyS7"
        # GPIO3_A4, header pin 16: bank 3, line 4.
        assert gnss.pps_gpio == 100
        assert divmod(gnss.pps_gpio, 32) == (3, 4)

    def test_sensor_hub_is_the_hub_firmware_id(self):
        hub = _profile("rock-5b-plus").sensor_hub
        assert hub is not None
        assert hub.usb_vid_pid.lower() == "1209:ad05"

    def test_no_board_level_battery_gauge(self):
        """The shunt belongs to the rig, so the board carries no gauge default."""
        assert _profile("rock-5b-plus").battery_gauge is None

    def test_buttons_are_header_gpio_numbers_clear_of_bound_lines(self):
        profile = _profile("rock-5b-plus")
        assert profile.buttons is not None
        # Header pins 29/31/33/35: GPIO1_D7, GPIO1_B7, GPIO3_A7, GPIO3_B6.
        assert profile.buttons.pins() == [63, 47, 103, 110]
        assert [divmod(p, 32) for p in profile.buttons.pins()] == [
            (1, 31),
            (1, 15),
            (3, 7),
            (3, 14),
        ]
        # Debug console, I2C8_M4, GNSS UART7_M1, SPI0_M2 panel lines, PPS.
        assert set(profile.reserved_lines()) >= {
            13, 14, 115, 114, 113, 112, 42, 41, 43, 44, 45, 100,
        }
        assert not set(profile.buttons.pins()) & set(profile.reserved_lines())

    def test_no_header_uart_is_offered_as_a_flight_controller(self):
        """The GNSS UART must never be probed as an FC link."""
        profile = _profile("rock-5b-plus")
        assert profile.uart_paths == []
        assert all(i.type == "usb-acm" for i in profile.flight_controller.interfaces)


class TestRock5CBindings:
    def test_single_csi_connector(self):
        cams = _profile("rock-5c-lite").cameras.supported
        assert [c.id for c in cams] == ["csi-cam0"]
        assert cams[0].bus == "csi2_dphy0"
        assert cams[0].overlay_ref == "rock-5a-radxa-camera-4k"

    def test_gnss_is_uart8_with_a_pps_line(self):
        gnss = _profile("rock-5c-lite").gnss
        assert gnss is not None
        assert gnss.transport == "serial"
        assert gnss.device == "/dev/ttyS8"
        # GPIO1_A5, header pin 16: bank 1, line 5.
        assert divmod(gnss.pps_gpio or 0, 32) == (1, 5)

    def test_no_board_level_battery_gauge(self):
        assert _profile("rock-5c-lite").battery_gauge is None

    def test_buttons_are_header_gpio_numbers_clear_of_bound_lines(self):
        profile = _profile("rock-5c-lite")
        assert profile.buttons is not None
        # Header pins 29/31/33/35: GPIO1_B2, GPIO1_B1, GPIO1_B4, GPIO4_A0.
        assert profile.buttons.pins() == [42, 41, 44, 128]
        assert not set(profile.buttons.pins()) & set(profile.reserved_lines())

    def test_every_panel_gpio_is_a_reserved_line(self):
        """The display bindings name pins as GPIO<bank>_<group><index>; each must
        be reserved so a button can never take it."""
        profile = _profile("rock-5c-lite")
        reserved = set(profile.reserved_lines())
        for display in profile.displays.supported:
            pins = list(display.gpio.values())
            if display.touch is not None:
                pins.append(display.touch.irq)
            for gpio in pins:
                bank, rest = gpio.pin.removeprefix("GPIO").split("_")
                number = int(bank) * 32 + "ABCD".index(rest[0]) * 8 + int(rest[1:])
                assert number in reserved, f"{display.id} {gpio.pin} ({number})"

    def test_no_sensor_hub_is_declared(self):
        assert _profile("rock-5c-lite").sensor_hub is None


class TestButtonValidation:
    _BASE = {
        "name": "Test",
        "reserved_gpio": [{"gpio": 13, "header_pin": 8, "use": "console"}],
        "gnss": {"transport": "serial", "pps_gpio": 100},
    }

    def test_clear_buttons_load(self):
        profile = BoardProfile(
            **self._BASE, buttons={"up": 63, "down": 47, "select": 103, "back": 110}
        )
        assert profile.reserved_lines() == [13, 100]

    @pytest.mark.parametrize(
        ("buttons", "message"),
        [
            ({"up": 13, "down": 47, "select": 103, "back": 110}, "13 is a reserved"),
            ({"up": 63, "down": 100, "select": 103, "back": 110}, "100 is a reserved"),
            ({"up": 63, "down": 63, "select": 103, "back": 110}, "63 is declared twice"),
        ],
    )
    def test_a_button_on_a_bound_line_is_rejected(self, buttons, message):
        with pytest.raises(ValueError, match=message):
            BoardProfile(**self._BASE, buttons=buttons)

    @pytest.mark.parametrize(
        "gauge",
        [
            {"type": "ina226", "i2c_bus": 8, "addr": 64},
            {"type": "ina226", "i2c_bus": 8, "addr": 64, "shunt_ohm": 0.001},
            {"type": "ina226", "i2c_bus": 8, "addr": 64, "max_current_a": 30.0},
            {
                "type": "ina226",
                "i2c_bus": 8,
                "addr": 64,
                "shunt_ohm": 0.0,
                "max_current_a": 30.0,
            },
        ],
    )
    def test_a_gauge_without_its_shunt_values_is_rejected(self, gauge):
        with pytest.raises(ValueError):
            BoardProfile(name="Test", battery_gauge=gauge)
