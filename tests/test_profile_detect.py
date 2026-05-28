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


@pytest.mark.parametrize(
    "fc_connected,expected",
    [
        (True, (0, 3, True)),
        (False, (0, 0, False)),
    ],
)
def test_fc_heartbeat_probe_reads_state_socket_v2_msgpack(
    monkeypatch, fc_connected: bool, expected: tuple[int, int, bool]
) -> None:
    """The probe also decodes the length-prefixed msgpack (v2) state wire.

    A v2 frame is `struct.pack("!I", len) + msgpack_body`; the leading
    length byte is 0x00, which is how the probe tells it apart from the
    v1 JSON line (leading `{`).
    """
    import struct

    msgpack = pytest.importorskip("msgpack")
    body = msgpack.packb({"fc_connected": fc_connected}, use_bin_type=True)
    payload = struct.pack("!I", len(body)) + body

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


def _route_sysnet(monkeypatch, sysnet_root: Path) -> None:
    """Reroute reads of /sys/class/net/* under a tmp tree.

    probe_uplink_type / probe_uplink_kinds construct ``Path("/sys/...")``
    directly, so we patch the local Path symbol with a wrapper that
    swaps the leading prefix.
    """
    real = Path

    def _Patched(arg, *args, **kwargs):
        if isinstance(arg, str) and arg.startswith("/sys/class/net"):
            return real(str(sysnet_root) + arg[len("/sys/class/net"):])
        return real(arg, *args, **kwargs)

    monkeypatch.setattr(profile_detect, "Path", _Patched)


def test_probe_uplink_kinds_wlan_only(tmp_path, monkeypatch) -> None:
    sysnet = tmp_path
    (sysnet / "wlan0").mkdir()
    (sysnet / "wlan0" / "operstate").write_text("up\n")
    _route_sysnet(monkeypatch, sysnet)
    assert profile_detect.probe_uplink_kinds() == ["WiFi"]
    assert profile_detect.probe_uplink_type() == (1, 0, True)


def test_probe_uplink_kinds_eth_only(tmp_path, monkeypatch) -> None:
    sysnet = tmp_path
    (sysnet / "eth0").mkdir()
    (sysnet / "eth0" / "carrier").write_text("1\n")
    _route_sysnet(monkeypatch, sysnet)
    assert profile_detect.probe_uplink_kinds() == ["ethernet"]


def test_probe_uplink_kinds_eth_and_wlan(tmp_path, monkeypatch) -> None:
    sysnet = tmp_path
    (sysnet / "eth0").mkdir()
    (sysnet / "eth0" / "carrier").write_text("1\n")
    (sysnet / "wlan0").mkdir()
    (sysnet / "wlan0" / "operstate").write_text("up\n")
    _route_sysnet(monkeypatch, sysnet)
    assert profile_detect.probe_uplink_kinds() == ["ethernet", "WiFi"]


def test_probe_uplink_kinds_dark(tmp_path, monkeypatch) -> None:
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect.probe_uplink_kinds() == []
    assert profile_detect.probe_uplink_type() == (0, 0, False)


# ---- probe_mavlink_serial: USB VID matching --------------------------------


class _FakeSerialPath:
    """Path stub for probe_mavlink_serial: only the configured tty
    reports as existing. Everything else returns False. We keep the
    rest of the Path API intact by inheriting from real Path."""

    def __init__(self, present_path: str):
        self._present = present_path

    def __call__(self, p):
        # Mirror Path's constructor signature: return an object whose
        # .exists() check uses the configured presence list, while
        # leaving other attributes (.name, etc.) intact.
        wrapped = Path(p)

        class _PathWithPresence:
            def __init__(_self, base, present):
                _self._base = base
                _self._present = present

            def exists(_self):
                return str(_self._base) == _self._present

            @property
            def name(_self):
                return _self._base.name

            def __getattr__(_self, item):
                return getattr(_self._base, item)

        return _PathWithPresence(wrapped, self._present)


def test_mavlink_serial_baseline_when_port_exists_but_vid_unknown(monkeypatch) -> None:
    """Generic USB-serial adapter at /dev/ttyACM0 with no known FC
    vendor still scores +3 air (the port-exists baseline)."""
    monkeypatch.setattr(
        profile_detect, "Path", _FakeSerialPath("/dev/ttyACM0")
    )
    monkeypatch.setattr(
        profile_detect, "_read_usb_vendor_for_tty", lambda _p: 0x1234
    )
    assert profile_detect.probe_mavlink_serial() == (0, 3, True)


def test_mavlink_serial_boosts_when_vid_matches_known_fc(monkeypatch) -> None:
    """SpeedyBee F4 v4 (pid.codes 0x1209) on /dev/ttyACM0 awards the
    +6 air-points score so a real FC outweighs hostname tiebreakers."""
    monkeypatch.setattr(
        profile_detect, "Path", _FakeSerialPath("/dev/ttyACM0")
    )
    monkeypatch.setattr(
        profile_detect, "_read_usb_vendor_for_tty", lambda _p: 0x1209
    )
    assert profile_detect.probe_mavlink_serial() == (0, 6, True)


def test_mavlink_serial_zero_when_no_ports(monkeypatch) -> None:
    """No tty present anywhere → zero contribution."""
    monkeypatch.setattr(
        profile_detect, "Path", _FakeSerialPath("/dev/nonexistent-zz")
    )
    assert profile_detect.probe_mavlink_serial() == (0, 0, False)


def test_mavlink_serial_uart_with_no_usb_metadata(monkeypatch) -> None:
    """SoC UART (e.g. /dev/ttyAMA0) has no /sys/class/tty/<n>/device
    USB ancestor. The helper returns None and the probe falls back to
    the +3 baseline rather than swallowing the signal entirely."""
    monkeypatch.setattr(
        profile_detect, "Path", _FakeSerialPath("/dev/ttyAMA0")
    )
    monkeypatch.setattr(
        profile_detect, "_read_usb_vendor_for_tty", lambda _p: None
    )
    assert profile_detect.probe_mavlink_serial() == (0, 3, True)


# ---- probe_rtl8812: USB vendor IDs of the RTL8812 family ------------------


def _stub_lsusb(monkeypatch, stdout: str, returncode: int = 0) -> None:
    """Replace subprocess.run with a stub that returns the given lsusb
    output. probe_rtl8812 only ever calls run() with argv[0]=='lsusb'
    so the stub is narrow on purpose."""
    class _Result:
        def __init__(self, rc, out):
            self.returncode = rc
            self.stdout = out

    def _fake_run(argv, *_args, **_kwargs):
        return _Result(returncode, stdout)

    monkeypatch.setattr(profile_detect.subprocess, "run", _fake_run)


def test_rtl8812_matches_canonical_pid(monkeypatch) -> None:
    """RTL8812EU canonical PID 0x8812 still scores."""
    _stub_lsusb(
        monkeypatch,
        "Bus 001 Device 002: ID 0bda:8812 Realtek Semiconductor Corp.\n",
    )
    assert profile_detect.probe_rtl8812() == (1, 1, True)


def test_rtl8812_matches_pid_a81a(monkeypatch) -> None:
    """Bench dev rig: groundnode's Realtek '802.11ac NIC' exposes
    ``0bda:a81a``. Same monitor-mode driver works; probe should score
    the adapter so the install-time profile decision isn't reduced to
    a hostname tiebreak."""
    _stub_lsusb(
        monkeypatch,
        "Bus 006 Device 004: ID 0bda:a81a Realtek Semiconductor Corp. 802.11ac NIC\n",
    )
    assert profile_detect.probe_rtl8812() == (1, 1, True)


def test_rtl8812_zero_when_no_realtek_adapter(monkeypatch) -> None:
    """No Realtek adapter present → zero contribution."""
    _stub_lsusb(
        monkeypatch,
        "Bus 001 Device 001: ID 1d6b:0002 Linux Foundation 2.0 root hub\n",
    )
    assert profile_detect.probe_rtl8812() == (0, 0, False)


def test_rtl8812_ignores_unrelated_realtek_pid(monkeypatch) -> None:
    """A Realtek NIC with a non-RTL8812 PID (e.g. an Ethernet adapter)
    should not match. Picks 0x8153 (RTL8153 USB Ethernet)."""
    _stub_lsusb(
        monkeypatch,
        "Bus 001 Device 003: ID 0bda:8153 Realtek Semiconductor Corp. RTL8153\n",
    )
    assert profile_detect.probe_rtl8812() == (0, 0, False)


# ---- Hostname is a soft tiebreaker, not a hardware override ---------------


def test_fc_with_matched_vid_outweighs_groundnode_hostname(monkeypatch) -> None:
    """The motivating fix: a Rock 5C named ``groundnode`` with an FC
    plugged in (pid.codes VID) and a WFB-ng dongle should auto-detect
    as ``drone`` because hardware reality (FC + matched VID) carries
    more weight than the stale hostname."""
    _stub_probes(
        monkeypatch,
        probe_mavlink_serial=(0, 6, True),  # FC with matched USB VID
        probe_rtl8812=(1, 1, True),  # WFB-ng dongle present
        probe_uplink_type=(1, 0, True),  # wlan up
    )
    monkeypatch.setattr(
        profile_detect, "_hostname_suggested_profile", lambda: "ground_station"
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "drone"
    # ground = rtl(1) + uplink(1) + hostname(2) = 4
    # air    = mavlink(6) + rtl(1) = 7
    assert result["ground_score"] == 4
    assert result["air_score"] == 7


def test_hostname_still_resolves_genuine_ground_station(monkeypatch) -> None:
    """Regression: a ground-station rig with no FC, OLED + buttons +
    WFB dongle + hostname ``groundnode`` still resolves to
    ``ground_station`` — hostname weight stays meaningful when the
    hardware signal is itself low."""
    _stub_probes(
        monkeypatch,
        probe_i2c_oled=(3, 0, True),
        probe_gpio_buttons=(2, 0, True),
        probe_rtl8812=(1, 1, True),
        probe_uplink_type=(1, 0, True),
    )
    monkeypatch.setattr(
        profile_detect, "_hostname_suggested_profile", lambda: "ground_station"
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "ground_station"
    # ground = oled(3) + buttons(2) + rtl(1) + uplink(1) + hostname(2) = 9
    # air    = rtl(1)
    assert result["ground_score"] == 9
    assert result["air_score"] == 1


def test_hostname_tiebreak_when_hardware_is_silent(monkeypatch) -> None:
    """No hardware probes fire; hostname ``groundnode`` still pushes
    the result toward ground_station via the +2 weight."""
    _stub_probes(monkeypatch)  # everything zero
    monkeypatch.setattr(
        profile_detect, "_hostname_suggested_profile", lambda: "ground_station"
    )
    result = profile_detect.detect_profile(config_override=None)
    assert result["profile"] == "ground_station"
    assert result["ground_score"] == 2
    assert result["air_score"] == 0


# ---- _detect_ethernet_iface: predictable network name handling -------------


def _make_iface(sysnet: Path, name: str, *, operstate: str | None = None) -> None:
    """Create a fake /sys/class/net/<name> directory under sysnet.

    If ``operstate`` is provided, write it to the ``operstate`` file.
    """
    iface_dir = sysnet / name
    iface_dir.mkdir(parents=True, exist_ok=True)
    if operstate is not None:
        (iface_dir / "operstate").write_text(operstate + "\n")


def test_detect_eth_only_eth0_up(tmp_path, monkeypatch) -> None:
    """Pi-class BSP: only eth0 exists and is up → returns eth0."""
    _make_iface(tmp_path, "eth0", operstate="up")
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() == "eth0"


def test_detect_eth_only_end1_up(tmp_path, monkeypatch) -> None:
    """Rock 5C BSP: only end1 exists and is up → returns end1."""
    _make_iface(tmp_path, "end1", operstate="up")
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() == "end1"


def test_detect_eth_eth0_and_end1_both_up_prefers_eth0(tmp_path, monkeypatch) -> None:
    """Both eth0 and end1 are up → eth* wins by pattern priority."""
    _make_iface(tmp_path, "eth0", operstate="up")
    _make_iface(tmp_path, "end1", operstate="up")
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() == "eth0"


def test_detect_eth_only_enp5s0_up(tmp_path, monkeypatch) -> None:
    """systemd predictable naming: enp5s0 alone → returns enp5s0."""
    _make_iface(tmp_path, "enp5s0", operstate="up")
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() == "enp5s0"


def test_detect_eth_end1_down_only_falls_back(tmp_path, monkeypatch) -> None:
    """end1 exists but is down, nothing else → returns end1 from the
    second-pass existence fallback."""
    _make_iface(tmp_path, "end1", operstate="down")
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() == "end1"


def test_detect_eth_nothing_returns_none(tmp_path, monkeypatch) -> None:
    """No matching interface anywhere → returns None."""
    # tmp_path is empty; only "lo" or unrelated NICs would exist in reality
    # but for this test the empty dir is the cleanest signal.
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() is None


def test_detect_eth_up_beats_fallback_across_patterns(tmp_path, monkeypatch) -> None:
    """end1 exists but is down; enp5s0 is up → returns enp5s0.

    The first pass should walk all patterns looking for an "up" interface
    before falling back to existence. This guards against the bug where
    the function would return the first existing iface from the eth*/end*
    pattern before looking at enp*/enx* for an "up" one.
    """
    _make_iface(tmp_path, "end1", operstate="down")
    _make_iface(tmp_path, "enp5s0", operstate="up")
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() == "enp5s0"


def test_detect_eth_multiple_eth_both_up_alphabetical(tmp_path, monkeypatch) -> None:
    """Multiple eth* both up → returns eth0 (alphabetical sort)."""
    _make_iface(tmp_path, "eth0", operstate="up")
    _make_iface(tmp_path, "eth1", operstate="up")
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect._detect_ethernet_iface() == "eth0"


# ---- probe_uplink_type / probe_uplink_kinds: predictable-name awareness ----


def test_probe_uplink_kinds_end1_only(tmp_path, monkeypatch) -> None:
    """Rock 5C BSP: end1 has carrier=1 → uplink_kinds reports ethernet."""
    sysnet = tmp_path
    (sysnet / "end1").mkdir()
    (sysnet / "end1" / "carrier").write_text("1\n")
    (sysnet / "end1" / "operstate").write_text("up\n")
    _route_sysnet(monkeypatch, sysnet)
    assert profile_detect.probe_uplink_kinds() == ["ethernet"]
    assert profile_detect.probe_uplink_type() == (1, 0, True)


def test_probe_uplink_type_no_ethernet_iface_at_all(tmp_path, monkeypatch) -> None:
    """No eth*/end*/enp*/enx* iface → ethernet contributes nothing.

    A wlan0 that is up still gets the (1, 0, True) score so this test
    deliberately leaves wlan absent too. Result is the dark (0, 0, False).
    """
    _route_sysnet(monkeypatch, tmp_path)
    assert profile_detect.probe_uplink_type() == (0, 0, False)
    assert profile_detect.probe_uplink_kinds() == []
