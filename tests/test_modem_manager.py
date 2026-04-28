"""Tests for the ground-station modem manager.

Covers construction, IMEI read paths, APN auto-detection, configuration
persistence, status reads, bring-up / bring-down state transitions, and
data-usage byte counters. dbus-next, mmcli output, and the AT-fallback
service are all mocked. No real modem, no real subprocess.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.core.subprocess import CmdResult
from ados.hal.modem import ModemInfo
from ados.services.ground_station import modem_manager as mm
from ados.services.ground_station.modem_manager import GroundStationModemManager


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def tmp_config_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect the module-level config path to a tmp file."""
    target = tmp_path / "ground-station-modem.json"
    monkeypatch.setattr(mm, "_CONFIG_PATH", target)
    return target


def _ok(stdout: str = "", stderr: str = "", rc: int = 0) -> CmdResult:
    return CmdResult(returncode=rc, stdout=stdout, stderr=stderr)


# ---------------------------------------------------------------------------
# Construction
# ---------------------------------------------------------------------------


def test_construction_default_state(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    assert manager._fallback_mode is False
    assert manager._dbus_fail_count == 0
    assert manager._brought_up is False
    assert manager._bus is None
    assert manager._last_status == {}
    assert isinstance(manager._config, dict)


def test_construction_loads_existing_config(tmp_config_path: Path) -> None:
    tmp_config_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_config_path.write_text(
        json.dumps({"apn": "jionet", "cap_gb": 5.0, "enabled": True}),
        encoding="utf-8",
    )
    manager = GroundStationModemManager()
    assert manager._config["apn"] == "jionet"
    assert manager._config["cap_gb"] == 5.0
    assert manager._config["enabled"] is True


# ---------------------------------------------------------------------------
# IMEI read
# ---------------------------------------------------------------------------


async def test_imei_read_via_mmcli_happy_path(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    list_output = "    /org/freedesktop/ModemManager1/Modem/0 [QUALCOMM]\n"
    detail_output = (
        "  -----------------------------\n"
        "  Hardware |   manufacturer: 'QUALCOMM'\n"
        "           |          model: 'SIM7600G-H'\n"
        "           |        revision: '1.0'\n"
        "           | equipment identifier: 869710030003456\n"
    )
    with patch.object(
        mm, "run_cmd", new=AsyncMock(side_effect=[_ok(list_output), _ok(detail_output)])
    ):
        imei = await manager._read_imei_via_dbus()
    assert imei == "869710030003456"


async def test_imei_read_no_modem_listed(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    with patch.object(mm, "run_cmd", new=AsyncMock(return_value=_ok(""))):
        imei = await manager._read_imei_via_dbus()
    assert imei is None


async def test_imei_read_mmcli_missing(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    with patch.object(mm, "run_cmd", new=AsyncMock(side_effect=FileNotFoundError())):
        imei = await manager._read_imei_via_dbus()
    assert imei is None


async def test_imei_read_no_digits_in_detail(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    list_output = "/Modem/0\n"
    detail_output = "  Hardware | equipment identifier: not-a-number\n"
    with patch.object(
        mm, "run_cmd", new=AsyncMock(side_effect=[_ok(list_output), _ok(detail_output)])
    ):
        imei = await manager._read_imei_via_dbus()
    assert imei is None


# ---------------------------------------------------------------------------
# APN auto-detect
# ---------------------------------------------------------------------------


async def test_apn_auto_detect_jio(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    list_output = "/Modem/0\n"
    modem_output = "  /SIM/3\n"
    sim_output = "  imsi: 405857123456789\n"
    with patch.object(
        mm,
        "run_cmd",
        new=AsyncMock(
            side_effect=[_ok(list_output), _ok(modem_output), _ok(sim_output)]
        ),
    ):
        apn = await manager._auto_detect_apn()
    assert apn == "jionet"


async def test_apn_auto_detect_airtel(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    list_output = "/Modem/0\n"
    modem_output = "  /SIM/0\n"
    sim_output = "  IMSI: 404100000000001\n"
    with patch.object(
        mm,
        "run_cmd",
        new=AsyncMock(
            side_effect=[_ok(list_output), _ok(modem_output), _ok(sim_output)]
        ),
    ):
        apn = await manager._auto_detect_apn()
    assert apn == "airtelgprs.com"


async def test_apn_auto_detect_unknown_imsi(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    list_output = "/Modem/0\n"
    modem_output = "  /SIM/0\n"
    sim_output = "  imsi: 999999999999999\n"
    with patch.object(
        mm,
        "run_cmd",
        new=AsyncMock(
            side_effect=[_ok(list_output), _ok(modem_output), _ok(sim_output)]
        ),
    ):
        apn = await manager._auto_detect_apn()
    assert apn is None


async def test_apn_auto_detect_no_sim_in_output(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    list_output = "/Modem/0\n"
    modem_output = "  no sim line here\n"
    with patch.object(
        mm,
        "run_cmd",
        new=AsyncMock(side_effect=[_ok(list_output), _ok(modem_output)]),
    ):
        apn = await manager._auto_detect_apn()
    assert apn is None


# ---------------------------------------------------------------------------
# configure() persistence
# ---------------------------------------------------------------------------


async def test_configure_writes_apn_to_disk(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    # Skip the side-effect bring_up by setting enabled=False up front.
    result = await manager.configure(apn="jionet", enabled=False)
    assert result["apn"] == "jionet"
    assert result["enabled"] is False
    on_disk = json.loads(tmp_config_path.read_text(encoding="utf-8"))
    assert on_disk["apn"] == "jionet"
    assert on_disk["enabled"] is False


async def test_configure_no_change_no_write(tmp_config_path: Path) -> None:
    tmp_config_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_config_path.write_text(
        json.dumps({"apn": "jionet", "enabled": False}), encoding="utf-8"
    )
    manager = GroundStationModemManager()
    mtime_before = tmp_config_path.stat().st_mtime_ns
    # Re-set the same values.
    await manager.configure(apn="jionet", enabled=False)
    mtime_after = tmp_config_path.stat().st_mtime_ns
    assert mtime_before == mtime_after


async def test_configure_cap_gb_only(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    result = await manager.configure(cap_gb=10.0)
    assert result["cap_gb"] == 10.0
    on_disk = json.loads(tmp_config_path.read_text(encoding="utf-8"))
    assert on_disk["cap_gb"] == 10.0


# ---------------------------------------------------------------------------
# status() / data_usage()
# ---------------------------------------------------------------------------


async def test_status_at_fallback_when_no_iface(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    manager._fallback_mode = True
    # Force `_current_iface` to return a path that does not exist on the
    # test host; `_status_at` reads /sys but tolerates missing entries.
    with patch.object(
        manager, "_current_iface", return_value="wwan-nonexistent-iface"
    ):
        with patch.object(
            mm, "run_cmd", new=AsyncMock(return_value=_ok(""))
        ):
            status = await manager.status()
    assert status["connected"] is False
    assert status["fallback_mode"] is True
    assert status["signal_quality"] == -1


async def test_data_usage_unavailable_iface(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    with patch.object(
        manager, "_current_iface", return_value="wwan-nonexistent-iface"
    ):
        usage = await manager.data_usage()
    assert usage["available"] is False
    assert usage["rx_bytes"] == 0
    assert usage["tx_bytes"] == 0
    assert usage["total_bytes"] == 0


async def test_data_usage_reads_byte_counters(
    tmp_path: Path, tmp_config_path: Path
) -> None:
    """When /sys/class/net/<iface>/statistics is readable, return real bytes."""
    iface_dir = tmp_path / "sysnet" / "test-iface" / "statistics"
    iface_dir.mkdir(parents=True)
    (iface_dir / "rx_bytes").write_text("12345\n")
    (iface_dir / "tx_bytes").write_text("6789\n")

    manager = GroundStationModemManager()

    real_path = mm.Path

    class FakePath:
        def __init__(self, *parts: str) -> None:
            joined = "/".join(str(p) for p in parts)
            if joined.startswith("/sys/class/net/test-iface"):
                tail = joined.replace("/sys/class/net/test-iface", "")
                self._inner = real_path(str(iface_dir.parent) + tail)
            else:
                self._inner = real_path(joined)

        def __truediv__(self, other: str) -> "FakePath":
            inst = FakePath.__new__(FakePath)
            inst._inner = self._inner / other
            return inst

        def read_text(self, *args: object, **kwargs: object) -> str:
            return self._inner.read_text(*args, **kwargs)

        def exists(self) -> bool:
            return self._inner.exists()

    with patch.object(manager, "_current_iface", return_value="test-iface"):
        with patch.object(mm, "Path", FakePath):
            usage = await manager.data_usage()
    assert usage["available"] is True
    assert usage["rx_bytes"] == 12345
    assert usage["tx_bytes"] == 6789
    assert usage["total_bytes"] == 12345 + 6789


# ---------------------------------------------------------------------------
# dbus failure / fallback
# ---------------------------------------------------------------------------


def test_register_dbus_failure_flips_to_fallback(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    for _ in range(mm._DBUS_FAIL_THRESHOLD):
        manager._register_dbus_failure("simulated")
    assert manager._fallback_mode is True
    assert manager._dbus_fail_count >= mm._DBUS_FAIL_THRESHOLD


def test_register_dbus_success_recovers_fallback(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    manager._fallback_mode = True
    manager._dbus_fail_count = 5
    manager._register_dbus_success()
    assert manager._fallback_mode is False
    assert manager._dbus_fail_count == 0


# ---------------------------------------------------------------------------
# close()
# ---------------------------------------------------------------------------


async def test_close_no_bus_is_idempotent(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    assert manager._bus is None
    # Should not raise.
    await manager.close()
    await manager.close()
    assert manager._bus is None


async def test_close_disconnects_bus(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    fake_bus = MagicMock()
    fake_bus.disconnect = MagicMock(return_value=None)
    manager._bus = fake_bus
    await manager.close()
    fake_bus.disconnect.assert_called_once()
    assert manager._bus is None


# ---------------------------------------------------------------------------
# probe()
# ---------------------------------------------------------------------------


async def test_probe_with_modem_detected(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    info = ModemInfo(
        name="SIM7600G-H",
        operator="Jio",
        signal_strength=72,
        connection_state="connected",
        ip_address="10.0.0.5",
    )
    with patch.object(mm, "detect_modem", return_value=info):
        with patch.object(
            manager, "_find_serial_port", new=AsyncMock(return_value="/dev/ttyUSB2")
        ):
            with patch.object(
                manager,
                "_read_imei_via_dbus",
                new=AsyncMock(return_value="869710030003456"),
            ):
                result = await manager.probe()
    assert result["detected"] is True
    assert result["model"] == "SIM7600G-H"
    assert result["device_path"] == "/dev/ttyUSB2"
    assert result["imei"] == "869710030003456"


async def test_probe_no_modem_anywhere(tmp_config_path: Path) -> None:
    manager = GroundStationModemManager()
    with patch.object(mm, "detect_modem", return_value=None):
        with patch.object(
            manager, "_find_serial_port", new=AsyncMock(return_value=None)
        ):
            with patch.object(
                manager, "_read_imei_via_dbus", new=AsyncMock(return_value=None)
            ):
                result = await manager.probe()
    assert result["detected"] is False
    assert result["model"] is None
    assert result["device_path"] is None
    assert result["imei"] is None
