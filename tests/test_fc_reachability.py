"""An MSP FC (Betaflight/iNav) is reachable and reads HEALTHY, even though it
never emits a MAVLink heartbeat, so the setup/health surfaces must not report it
as DEGRADED / "needs action". Regression guard for the four-firmware universal
accuracy fix.
"""

from ados.api.runtime import FcStatus
from ados.setup.hardware_check import _check_fc


class _FakeRuntime:
    def __init__(self, fc: FcStatus) -> None:
        self._fc = fc

    def fc_status(self) -> FcStatus:
        return self._fc


def test_fcstatus_reachable_for_identified_msp_fc() -> None:
    # A Betaflight FC: no MAVLink heartbeat, but transport open + variant known.
    fc = FcStatus(
        connected=False,
        transport_open=True,
        mavlink_alive=False,
        fc_variant="betaflight",
        fc_firmware="betaflight",
    )
    assert fc.reachable is True


def test_fcstatus_reachable_for_msp_detected_hint() -> None:
    fc = FcStatus(connected=False, transport_open=True, fc_link_hint="msp_detected")
    assert fc.reachable is True


def test_fcstatus_not_reachable_when_transport_closed() -> None:
    fc = FcStatus(connected=False, transport_open=False, fc_variant="betaflight")
    assert fc.reachable is False


def test_fcstatus_reachable_for_live_mavlink() -> None:
    fc = FcStatus(connected=True, transport_open=True, mavlink_alive=True)
    assert fc.reachable is True


def test_check_fc_reports_ok_for_reachable_msp_fc() -> None:
    fc = FcStatus(
        connected=False,
        transport_open=True,
        mavlink_alive=False,
        fc_variant="betaflight",
        fc_firmware="betaflight",
        port="/dev/ttyACM0",
        baud=115200,
    )
    item = _check_fc(_FakeRuntime(fc))
    assert item.state == "ok"
    assert "Betaflight" in item.label
    assert "(MSP)" in item.label


def test_check_fc_warning_is_firmware_neutral(monkeypatch) -> None:
    # No reachable FC + a serial device present → a firmware-neutral warning,
    # never the old MAVLink-only "heartbeat not yet detected" text.
    fc = FcStatus(connected=False, transport_open=False)
    monkeypatch.setattr(
        "ados.bootstrap.profile_detect.probe_mavlink_serial",
        lambda: (None, None, True),
    )
    item = _check_fc(_FakeRuntime(fc))
    assert item.state == "warning"
    assert "MAVLink heartbeat not yet detected" not in (item.detail or "")
