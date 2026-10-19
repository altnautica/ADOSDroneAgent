"""Visor outcome of the boot-time profile detector and the GNSS/OLED rule."""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest
import yaml

from ados.bootstrap import _profile_detect_visor, profile_detect

V = _profile_detect_visor
# A stock ArduPilot FC on USB: matched vendor serial port plus a heartbeat.
FC = {"probe_mavlink_serial": (0, 6, True), "probe_fc_heartbeat": (0, 3, True)}
WFB = {"probe_rtl8812": (1, 1, True)}


def _stub(monkeypatch, *, hub: str = "", prior=None, **points) -> None:
    defaults = {
        "probe_i2c_oled": (0, 0, False),
        "probe_gpio_buttons": (0, 0, False),
        "probe_rtl8812": (0, 0, False),
        "probe_mavlink_serial": (0, 0, False),
        "probe_gps_serial": (0, 0, False),
        "probe_fc_heartbeat": (0, 0, False),
        "probe_uplink_type": (0, 0, False),
    }
    defaults.update(points)
    for name, value in defaults.items():
        monkeypatch.setattr(profile_detect, name, lambda v=value: v)
    monkeypatch.setattr(profile_detect, "probe_sensor_hub", lambda: hub)
    monkeypatch.setattr(profile_detect, "_hostname_suggested_profile", lambda: None)
    monkeypatch.setattr(
        profile_detect, "_read_last_known_profile", lambda path=None: prior
    )


def _stub_lsusb(monkeypatch, stdout: str, returncode: int = 0) -> None:
    def _run(argv, *_a, **_k):
        return SimpleNamespace(returncode=returncode, stdout=stdout)

    monkeypatch.setattr(V.subprocess, "run", _run)


# ---- probe_sensor_hub --------------------------------------------------------


def test_sensor_hub_firmware_is_classified(monkeypatch) -> None:
    _stub_lsusb(monkeypatch, "Bus 001 Device 004: ID 1209:ad05 ADOS Sensor Hub\n")
    monkeypatch.setattr(V, "probe_csi_cameras", lambda: pytest.fail("not needed"))
    assert V.probe_sensor_hub() == V.HUB_FIRMWARE


@pytest.mark.parametrize(
    ("cameras", "expected"),
    [(2, V.HUB_ARDUPILOT_MULTICAM), (3, V.HUB_ARDUPILOT_MULTICAM), (1, "")],
)
def test_ardupilot_usb_counts_only_with_two_csi_cameras(monkeypatch, cameras, expected) -> None:
    _stub_lsusb(monkeypatch, "Bus 001 Device 005: ID 1209:5741 ArduPilot\n")
    monkeypatch.setattr(V, "probe_csi_cameras", lambda: cameras)
    assert V.probe_sensor_hub() == expected


def test_sensor_hub_empty_without_matching_device(monkeypatch) -> None:
    _stub_lsusb(monkeypatch, "Bus 001 Device 002: ID 0bda:8812 Realtek\n")
    monkeypatch.setattr(V, "probe_csi_cameras", lambda: pytest.fail("not needed"))
    assert V.probe_sensor_hub() == ""


def test_sensor_hub_empty_when_lsusb_missing(monkeypatch) -> None:
    def _missing(*_a, **_k):
        raise FileNotFoundError("lsusb")

    monkeypatch.setattr(V.subprocess, "run", _missing)
    assert V.probe_sensor_hub() == ""


# ---- detect_profile ----------------------------------------------------------


def test_sensor_hub_firmware_is_visor_over_its_own_serial_and_heartbeat(monkeypatch) -> None:
    _stub(monkeypatch, hub=V.HUB_FIRMWARE, **FC)
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "visor"
    assert result["source"] == "detected"
    assert result["air_score"] == 0
    assert result["visor_score"] > result["air_score"]
    assert result["signals"]["sensor_hub"] is True


def test_sensor_hub_firmware_is_visor_even_next_to_wfb_and_gps(monkeypatch) -> None:
    _stub(monkeypatch, hub=V.HUB_FIRMWARE, probe_gps_serial=(0, 2, True), **FC, **WFB)
    assert profile_detect.detect_profile(config_override=None)["profile"] == "visor"


def test_stage_a_ardupilot_multicam_without_wfb_is_visor(monkeypatch) -> None:
    _stub(monkeypatch, hub=V.HUB_ARDUPILOT_MULTICAM, probe_gps_serial=(0, 2, True), **FC)
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "visor"
    assert result["source"] == "detected"


def test_stage_a_ardupilot_multicam_with_wfb_is_a_stereo_drone(monkeypatch) -> None:
    _stub(monkeypatch, hub=V.HUB_ARDUPILOT_MULTICAM, **FC, **WFB)
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "drone"
    assert result["visor_score"] == 0
    assert result["air_score"] == 10


def test_ardupilot_fc_without_cameras_stays_drone(monkeypatch) -> None:
    _stub(monkeypatch, hub="", **FC)
    assert profile_detect.detect_profile(config_override=None)["profile"] == "drone"


def test_visor_override_short_circuits(monkeypatch) -> None:
    monkeypatch.setattr(
        profile_detect, "probe_sensor_hub", lambda: pytest.fail("override skips probes")
    )
    result = profile_detect.detect_profile(config_override="visor")
    assert result["profile"] == "visor"
    assert result["source"] == "override"
    assert result["visor_score"] == 0


# ---- GNSS scoring ------------------------------------------------------------


def test_gps_fix_scores_air_without_oled(monkeypatch) -> None:
    _stub(monkeypatch, probe_gps_serial=(0, 2, True))
    result = profile_detect.detect_profile(config_override=None)
    assert result["air_score"] == 2
    assert result["profile"] == "drone"
    assert result["source"] == "detected"


def test_gps_fix_scores_nothing_next_to_oled(monkeypatch) -> None:
    _stub(monkeypatch, probe_gps_serial=(0, 2, True), probe_i2c_oled=(3, 0, True))
    result = profile_detect.detect_profile(config_override=None)
    assert result["air_score"] == 0
    assert result["ground_score"] == 3
    assert result["signals"]["gps_serial"] is True


# ---- persistence -------------------------------------------------------------


def test_write_profile_conf_records_visor(tmp_path: Path) -> None:
    target = tmp_path / "profile.conf"
    target.write_text("channel: stable\n", encoding="utf-8")
    assert profile_detect.write_profile_conf("visor", path=str(target)) is True
    doc = yaml.safe_load(target.read_text(encoding="utf-8"))
    assert doc == {"channel": "stable", "profile": "visor"}
    assert profile_detect._read_last_known_profile(str(target)) == "visor"


def test_write_profile_conf_keeps_explicit_workstation(tmp_path: Path) -> None:
    target = tmp_path / "profile.conf"
    target.write_text("profile: workstation\n", encoding="utf-8")
    assert profile_detect.write_profile_conf("visor", path=str(target)) is False
    assert yaml.safe_load(target.read_text(encoding="utf-8")) == {"profile": "workstation"}
