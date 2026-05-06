"""Tests for the boot-time profile auto-detect.

Covers the decision tail (strict argmax, persistence tiebreaker, drone
default), the override fast path, and the new `source` field in the
result. Probe internals are mocked out so the tests are pure-Python and
require no hardware.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from ados.bootstrap import profile_detect


def _stub_probes(monkeypatch, **points: tuple[int, int, bool]) -> None:
    """Replace each named probe with a callable returning the given tuple.

    Any probe not listed in ``points`` reports zero contribution.
    """
    defaults: dict[str, tuple[int, int, bool]] = {
        "probe_i2c_oled": (0, 0, False),
        "probe_gpio_buttons": (0, 0, False),
        "probe_rtl8812": (0, 0, False),
        "probe_mavlink_serial": (0, 0, False),
        "probe_gps_serial": (0, 0, False),
        "probe_fc_heartbeat": (0, 0, False),
        "probe_uplink_type": (0, 0, False),
    }
    for name, value in points.items():
        defaults[name] = value
    for name, value in defaults.items():
        monkeypatch.setattr(profile_detect, name, lambda v=value: v)
    monkeypatch.setattr(profile_detect, "probe_mesh_capable", lambda: False)


def test_override_short_circuits_probes(monkeypatch) -> None:
    called: list[str] = []

    def _explode() -> tuple[int, int, bool]:
        called.append("probed")
        return 0, 0, False

    monkeypatch.setattr(profile_detect, "probe_i2c_oled", _explode)

    result = profile_detect.detect_profile(config_override="ground_station")
    assert result["profile"] == "ground_station"
    assert result["source"] == "override"
    assert result["ground_score"] == 0
    assert result["air_score"] == 0
    assert called == []


def test_argmax_picks_drone_on_air_dominance(monkeypatch) -> None:
    _stub_probes(
        monkeypatch,
        probe_mavlink_serial=(0, 3, True),
        probe_fc_heartbeat=(0, 3, True),
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "drone"
    assert result["source"] == "detected"
    assert result["air_score"] == 6
    assert result["ground_score"] == 0


def test_argmax_picks_ground_station_on_ground_dominance(monkeypatch) -> None:
    _stub_probes(
        monkeypatch,
        probe_i2c_oled=(3, 0, True),
        probe_gpio_buttons=(2, 0, True),
        probe_uplink_type=(1, 0, True),
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "ground_station"
    assert result["source"] == "detected"
    assert result["ground_score"] == 6
    assert result["air_score"] == 0


def test_argmax_ground_wins_over_ambiguous_rtl8812_alone(monkeypatch) -> None:
    _stub_probes(
        monkeypatch,
        probe_rtl8812=(1, 1, True),
        probe_uplink_type=(1, 0, True),
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "ground_station"
    assert result["source"] == "detected"


def test_tied_scores_use_persisted_profile_as_tiebreaker(
    monkeypatch, tmp_path: Path
) -> None:
    _stub_probes(
        monkeypatch,
        probe_rtl8812=(1, 1, True),
    )
    monkeypatch.setattr(
        profile_detect,
        "_read_last_known_profile",
        lambda path=str(profile_detect.PROFILE_CONF): "ground_station",
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "ground_station"
    assert result["source"] == "tiebreaker"


def test_tied_scores_with_no_prior_default_to_drone(monkeypatch) -> None:
    _stub_probes(monkeypatch)  # everything zero
    monkeypatch.setattr(
        profile_detect,
        "_read_last_known_profile",
        lambda path=str(profile_detect.PROFILE_CONF): None,
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "drone"
    assert result["source"] == "default"
    assert result["air_score"] == 0
    assert result["ground_score"] == 0


def test_result_never_returns_legacy_unconfigured(monkeypatch) -> None:
    """Regression: the threshold ladder used to fall through to
    `unconfigured`. With strict argmax + persistence + drone default,
    every code path through detect_profile produces a usable profile."""
    _stub_probes(monkeypatch)  # everything zero, no prior
    monkeypatch.setattr(
        profile_detect,
        "_read_last_known_profile",
        lambda path=str(profile_detect.PROFILE_CONF): None,
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] in ("drone", "ground_station")


def test_result_carries_signals_and_mesh_flag(monkeypatch) -> None:
    _stub_probes(
        monkeypatch,
        probe_i2c_oled=(3, 0, True),
        probe_uplink_type=(1, 0, True),
    )
    monkeypatch.setattr(profile_detect, "probe_mesh_capable", lambda: True)
    result = profile_detect.detect_profile(config_override=None)
    assert result["signals"]["oled_i2c"] is True
    assert result["signals"]["uplink"] is True
    assert result["signals"]["mavlink_serial"] is False
    assert result["mesh_capable"] is True


@pytest.mark.parametrize(
    "fc_connected,expected",
    [
        (True, (0, 3, True)),
        (False, (0, 0, False)),
    ],
)
def test_fc_heartbeat_probe_reads_state_socket(
    monkeypatch, fc_connected: bool, expected: tuple[int, int, bool]
) -> None:
    """The heartbeat probe is a unix-socket consumer. Mock socket.socket
    to return a fake whose recv() yields a single JSON snapshot, then
    verify the probe parses ``fc_connected`` correctly."""
    import json

    payload = json.dumps({"fc_connected": fc_connected}).encode() + b"\n"

    class _FakeSocket:
        def __init__(self, *args, **kwargs):
            self._buf = payload
            self._closed = False

        def settimeout(self, _t):
            pass

        def connect(self, _addr):
            pass

        def recv(self, n: int) -> bytes:
            if self._closed or not self._buf:
                return b""
            chunk, self._buf = self._buf[:n], self._buf[n:]
            return chunk

        def close(self):
            self._closed = True

    class _PathStub:
        def __init__(self, p):
            self._p = str(p)

        def exists(self) -> bool:
            return self._p == "/run/ados/state.sock"

    monkeypatch.setattr(profile_detect, "Path", _PathStub)
    monkeypatch.setattr(profile_detect.socket, "socket", _FakeSocket)

    result = profile_detect.probe_fc_heartbeat(timeout=1.0)
    assert result == expected


def test_fc_heartbeat_probe_returns_zero_when_socket_missing(monkeypatch) -> None:
    class _NoFile:
        def __init__(self, _p):
            pass

        def exists(self) -> bool:
            return False

    monkeypatch.setattr(profile_detect, "Path", _NoFile)
    assert profile_detect.probe_fc_heartbeat(timeout=0.1) == (0, 0, False)
