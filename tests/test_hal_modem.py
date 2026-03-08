"""Tests for HAL cellular modem detection."""

from __future__ import annotations

import subprocess
from unittest.mock import MagicMock, patch

import pytest

from ados.hal.modem import (
    ModemInfo,
    _extract_field,
    _parse_modem_details,
    _run_mmcli,
    detect_modem,
    get_modem_status,
)


SAMPLE_MMCLI_M_OUTPUT = """\
  -------------------------
  General  |       model: QUECTEL RM500Q-GL
           |    revision: RM500QGLABR11A06M4G
  -------------------------
  Status   |       state: connected
           |    operator name: Jio
           |    signal quality: 72% (recent)
  -------------------------
  Bearer   |      address: 10.45.23.1
"""

SAMPLE_MMCLI_LIST_OUTPUT = """\
Found 1 modems:
    /org/freedesktop/ModemManager1/Modem/0 [QUECTEL] RM500Q-GL
"""


class TestModemInfo:
    def test_to_dict(self):
        info = ModemInfo(
            name="TestModem",
            operator="Airtel",
            signal_strength=85,
            connection_state="connected",
            ip_address="10.0.0.5",
        )
        d = info.to_dict()
        assert d["name"] == "TestModem"
        assert d["signal_strength"] == 85


class TestExtractField:
    def test_extract_existing_field(self):
        assert _extract_field(SAMPLE_MMCLI_M_OUTPUT, "model") == "QUECTEL RM500Q-GL"

    def test_extract_state(self):
        assert _extract_field(SAMPLE_MMCLI_M_OUTPUT, "state") == "connected"

    def test_extract_missing_field(self):
        assert _extract_field(SAMPLE_MMCLI_M_OUTPUT, "nonexistent") == ""

    def test_extract_signal_quality(self):
        result = _extract_field(SAMPLE_MMCLI_M_OUTPUT, "signal quality")
        assert "72" in result


class TestParseModemDetails:
    def test_parse_full_output(self):
        info = _parse_modem_details(SAMPLE_MMCLI_M_OUTPUT)
        assert info.name == "QUECTEL RM500Q-GL"
        assert info.connection_state == "connected"
        assert info.signal_strength == 72
        assert info.ip_address == "10.45.23.1"

    def test_parse_empty_output(self):
        info = _parse_modem_details("")
        assert info.name == "Unknown Modem"
        assert info.signal_strength == 0
        assert info.connection_state == "unknown"


class TestRunMmcli:
    def test_mmcli_not_found(self):
        with patch("ados.hal.modem.subprocess.run", side_effect=FileNotFoundError):
            assert _run_mmcli(["-L"]) is None

    def test_mmcli_success(self):
        mock_result = MagicMock()
        mock_result.returncode = 0
        mock_result.stdout = "some output"
        with patch("ados.hal.modem.subprocess.run", return_value=mock_result):
            assert _run_mmcli(["-L"]) == "some output"

    def test_mmcli_failure(self):
        mock_result = MagicMock()
        mock_result.returncode = 1
        with patch("ados.hal.modem.subprocess.run", return_value=mock_result):
            assert _run_mmcli(["-L"]) is None

    def test_mmcli_timeout(self):
        with patch(
            "ados.hal.modem.subprocess.run",
            side_effect=subprocess.TimeoutExpired(cmd="mmcli", timeout=10),
        ):
            assert _run_mmcli(["-L"]) is None


class TestDetectModem:
    def test_non_linux_returns_none(self):
        with patch("ados.hal.modem.platform.system", return_value="Darwin"):
            assert detect_modem() is None

    def test_no_modems_found(self):
        with (
            patch("ados.hal.modem.platform.system", return_value="Linux"),
            patch("ados.hal.modem._run_mmcli", return_value="No modems found"),
        ):
            assert detect_modem() is None

    def test_modem_found(self):
        with (
            patch("ados.hal.modem.platform.system", return_value="Linux"),
            patch("ados.hal.modem._run_mmcli") as mock_run,
        ):
            mock_run.side_effect = [
                SAMPLE_MMCLI_LIST_OUTPUT,
                SAMPLE_MMCLI_M_OUTPUT,
            ]
            info = detect_modem()
            assert info is not None
            assert info.name == "QUECTEL RM500Q-GL"

    def test_mmcli_not_available(self):
        with (
            patch("ados.hal.modem.platform.system", return_value="Linux"),
            patch("ados.hal.modem._run_mmcli", return_value=None),
        ):
            assert detect_modem() is None


class TestGetModemStatus:
    def test_non_linux(self):
        with patch("ados.hal.modem.platform.system", return_value="Darwin"):
            assert get_modem_status(0) is None

    def test_valid_modem(self):
        with (
            patch("ados.hal.modem.platform.system", return_value="Linux"),
            patch("ados.hal.modem._run_mmcli", return_value=SAMPLE_MMCLI_M_OUTPUT),
        ):
            info = get_modem_status(0)
            assert info is not None
            assert info.signal_strength == 72
