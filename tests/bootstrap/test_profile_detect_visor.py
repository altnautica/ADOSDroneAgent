"""Visor outcome of the boot-time profile detector and the GNSS/OLED rule."""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

import pytest
import yaml

from ados.bootstrap import _profile_detect_visor, profile_detect


def _stub(monkeypatch, *, hub: tuple[int, bool] = (0, False), prior=None, **points) -> None:
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

    monkeypatch.setattr(_profile_detect_visor.subprocess, "run", _run)


# ---- probe_sensor_hub --------------------------------------------------------


def test_sensor_hub_firmware_is_strong_visor_signal(monkeypatch) -> None:
    _stub_lsusb(monkeypatch, "Bus 001 Device 004: ID 1209:ad05 ADOS Sensor Hub\n")
    monkeypatch.setattr(
        _profile_detect_visor, "probe_csi_cameras", lambda: pytest.fail("not needed")
    )
    assert _profile_detect_visor.probe_sensor_hub() == (
        _profile_detect_visor.SENSOR_HUB_VISOR_POINTS,
        True,
    )


@pytest.mark.parametrize(("cameras", "expected"), [(2, (2, True)), (3, (2, True)), (1, (0, False))])
def test_ardupilot_usb_counts_only_with_two_csi_cameras(monkeypatch, cameras, expected) -> None:
    _stub_lsusb(monkeypatch, "Bus 001 Device 005: ID 1209:5741 ArduPilot\n")
    monkeypatch.setattr(_profile_detect_visor, "probe_csi_cameras", lambda: cameras)
    assert _profile_detect_visor.probe_sensor_hub() == expected


def test_sensor_hub_zero_without_matching_device(monkeypatch) -> None:
    _stub_lsusb(monkeypatch, "Bus 001 Device 002: ID 0bda:8812 Realtek\n")
    monkeypatch.setattr(
        _profile_detect_visor, "probe_csi_cameras", lambda: pytest.fail("not needed")
    )
    assert _profile_detect_visor.probe_sensor_hub() == (0, False)


def test_sensor_hub_zero_when_lsusb_missing(monkeypatch) -> None:
    def _missing(*_a, **_k):
        raise FileNotFoundError("lsusb")

    monkeypatch.setattr(_profile_detect_visor.subprocess, "run", _missing)
    assert _profile_detect_visor.probe_sensor_hub() == (0, False)


# ---- detect_profile ----------------------------------------------------------


def test_sensor_hub_outweighs_its_own_mavlink_serial_and_heartbeat(monkeypatch) -> None:
    _stub(
        monkeypatch,
        hub=(_profile_detect_visor.SENSOR_HUB_VISOR_POINTS, True),
        probe_mavlink_serial=(0, 6, True),
        probe_fc_heartbeat=(0, 3, True),
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "visor"
    assert result["source"] == "detected"
    assert result["visor_score"] == _profile_detect_visor.SENSOR_HUB_VISOR_POINTS
    assert result["air_score"] == 9
    assert result["signals"]["sensor_hub"] is True


def test_ardupilot_multicam_alone_reads_as_visor(monkeypatch) -> None:
    _stub(monkeypatch, hub=(2, True))
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "visor"


def test_visor_tie_uses_persisted_profile(monkeypatch) -> None:
    _stub(monkeypatch, hub=(2, True), prior="visor", probe_gps_serial=(0, 2, True))
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "visor"
    assert result["source"] == "tiebreaker"


def test_tie_prior_outside_leaders_falls_back_to_default(monkeypatch) -> None:
    _stub(monkeypatch, hub=(2, True), prior="ground_station", probe_gps_serial=(0, 2, True))
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "drone"
    assert result["source"] == "default"


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
