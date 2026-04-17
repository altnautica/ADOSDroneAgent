"""Tests for the MAVLink signing helpers."""

from __future__ import annotations

from dataclasses import dataclass
from unittest.mock import MagicMock

import pytest

from ados.core.config import ADOSConfig, MavlinkConfig
from ados.services.mavlink.signing import (
    FrameObserver,
    _fingerprint,
    detect_capability,
    disable_on_fc,
    enroll_fc,
    parse_key_hex,
    set_require,
)


# ──────────────────────────────────────────────────────────────
# parse_key_hex
# ──────────────────────────────────────────────────────────────

def test_parse_key_hex_accepts_64_hex_chars():
    key = parse_key_hex("00" * 32)
    assert isinstance(key, bytearray)
    assert len(key) == 32
    assert all(b == 0 for b in key)


def test_parse_key_hex_rejects_short_input():
    with pytest.raises(ValueError, match="64 hex chars"):
        parse_key_hex("aa" * 31)


def test_parse_key_hex_rejects_long_input():
    with pytest.raises(ValueError, match="64 hex chars"):
        parse_key_hex("aa" * 33)


def test_parse_key_hex_rejects_non_hex():
    with pytest.raises(ValueError, match="not valid hex"):
        parse_key_hex("z" + "a" * 63)


def test_parse_key_hex_rejects_non_string():
    with pytest.raises(ValueError, match="must be a string"):
        parse_key_hex(12345)  # type: ignore[arg-type]


# ──────────────────────────────────────────────────────────────
# _fingerprint
# ──────────────────────────────────────────────────────────────

def test_fingerprint_is_8_hex_chars():
    fp = _fingerprint(bytes(32))
    assert len(fp) == 8
    assert all(c in "0123456789abcdef" for c in fp)


def test_fingerprint_is_deterministic():
    assert _fingerprint(b"a" * 32) == _fingerprint(b"a" * 32)


def test_fingerprint_changes_with_key():
    a = _fingerprint(b"a" * 32)
    b = _fingerprint(b"b" * 32)
    assert a != b


# ──────────────────────────────────────────────────────────────
# detect_capability
# ──────────────────────────────────────────────────────────────

def test_capability_fc_not_connected():
    result = detect_capability(None, None, None)
    assert result["supported"] is False
    assert result["reason"] == "fc_not_connected"


def test_capability_fc_disconnected():
    fc = MagicMock()
    fc.connected = False
    result = detect_capability(fc, None, None)
    assert result["supported"] is False
    assert result["reason"] == "fc_not_connected"


def test_capability_rejects_px4():
    fc = MagicMock()
    fc.connected = True
    vs = MagicMock()
    vs.autopilot = 12  # MAV_AUTOPILOT_PX4
    result = detect_capability(fc, vs, None)
    assert result["supported"] is False
    assert result["reason"] == "firmware_px4_no_persistent_store"


def test_capability_rejects_unknown_autopilot():
    fc = MagicMock()
    fc.connected = True
    vs = MagicMock()
    vs.autopilot = 99
    result = detect_capability(fc, vs, None)
    assert result["supported"] is False
    assert result["reason"] == "firmware_not_supported"


def test_capability_ardupilot_without_signing_params():
    fc = MagicMock()
    fc.connected = True
    vs = MagicMock()
    vs.autopilot = 3  # MAV_AUTOPILOT_ARDUPILOTMEGA
    pc = MagicMock()
    pc.get_all.return_value = {"SYSID_THISMAV": 1.0, "BATT_CAPACITY": 5200.0}
    result = detect_capability(fc, vs, pc)
    assert result["supported"] is False
    assert result["reason"] == "firmware_too_old"


def test_capability_ardupilot_with_signing_params():
    fc = MagicMock()
    fc.connected = True
    vs = MagicMock()
    vs.autopilot = 3
    pc = MagicMock()
    pc.get_all.return_value = {
        "SYSID_THISMAV": 1.0,
        "SIGNING_REQUIRE": 0.0,
    }
    result = detect_capability(fc, vs, pc)
    assert result["supported"] is True
    assert result["reason"] == "ok"
    assert result["firmware_name"] == "ArduPilot"
    assert result["signing_params_present"] is True


# ──────────────────────────────────────────────────────────────
# enroll_fc zeroize discipline
# ──────────────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_enroll_fc_zeroizes_key_on_success():
    fc = MagicMock()
    fc.connected = True
    fc.connection = MagicMock()
    fc.connection.mav = MagicMock()

    key = bytearray(b"A" * 32)
    result = await enroll_fc(fc, key)

    # Key buffer must be zeroized regardless of outcome.
    assert all(b == 0 for b in key), "enroll_fc must zeroize the key buffer"
    assert result["success"] is True
    assert len(result["key_id"]) == 8


@pytest.mark.asyncio
async def test_enroll_fc_zeroizes_key_on_exception():
    fc = MagicMock()
    fc.connected = True
    fc.connection = MagicMock()
    fc.connection.mav = MagicMock()
    fc.connection.mav.setup_signing_send.side_effect = RuntimeError("radio dropped")

    key = bytearray(b"B" * 32)
    with pytest.raises(RuntimeError):
        await enroll_fc(fc, key)

    assert all(b == 0 for b in key), "enroll_fc must zeroize even on exception"


@pytest.mark.asyncio
async def test_enroll_fc_rejects_wrong_length_key():
    fc = MagicMock()
    fc.connected = True
    fc.connection = MagicMock()

    with pytest.raises(ValueError, match="32 bytes"):
        await enroll_fc(fc, bytearray(16))


@pytest.mark.asyncio
async def test_enroll_fc_rejects_disconnected_fc():
    fc = MagicMock()
    fc.connected = False
    fc.connection = None

    with pytest.raises(RuntimeError, match="not connected"):
        await enroll_fc(fc, bytearray(32))


# ──────────────────────────────────────────────────────────────
# disable_on_fc and set_require
# ──────────────────────────────────────────────────────────────

def test_disable_on_fc_sends_zero_key():
    fc = MagicMock()
    fc.connected = True
    fc.connection = MagicMock()
    mav = MagicMock()
    fc.connection.mav = mav

    result = disable_on_fc(fc)
    assert result["success"] is True

    # Called with all-zero 32-byte key and zero timestamp.
    args = mav.setup_signing_send.call_args
    _target_sys, _target_comp, key, ts = args.args
    assert key == bytes(32)
    assert ts == 0


def test_set_require_writes_param():
    fc = MagicMock()
    fc.connected = True
    fc.connection = MagicMock()
    mav = MagicMock()
    fc.connection.mav = mav

    result = set_require(fc, True)
    assert result["success"] is True
    assert result["require"] is True

    # param_set_send called with SIGNING_REQUIRE and value 1.0.
    args = mav.param_set_send.call_args
    _ts, _tc, name, value, _ptype = args.args
    assert name == b"SIGNING_REQUIRE"
    assert value == 1.0


# ──────────────────────────────────────────────────────────────
# FrameObserver
# ──────────────────────────────────────────────────────────────

def _build_v2_frame(signed: bool) -> bytes:
    """Minimal valid-looking v2 frame for observer tests."""
    inc_flags = 0x01 if signed else 0x00
    return bytes([0xFD, 0, inc_flags, 0, 0, 255, 190, 0, 0, 0])


def test_frame_observer_counts_signed_tx():
    obs = FrameObserver()
    obs.observe_frame(_build_v2_frame(signed=True), "tx")
    obs.observe_frame(_build_v2_frame(signed=True), "tx")
    obs.observe_frame(_build_v2_frame(signed=False), "tx")
    assert obs.tx_signed_count == 2


def test_frame_observer_counts_signed_rx():
    obs = FrameObserver()
    obs.observe_frame(_build_v2_frame(signed=True), "rx")
    assert obs.rx_signed_count == 1
    assert obs.last_signed_rx_at is not None


def test_frame_observer_ignores_v1_frames():
    obs = FrameObserver()
    # v1 frames have STX 0xFE (and are only 6 header bytes), but observing
    # them must not increment any counter.
    obs.observe_frame(bytes([0xFE] + [0] * 10), "tx")
    assert obs.tx_signed_count == 0


def test_frame_observer_ignores_short_frames():
    obs = FrameObserver()
    obs.observe_frame(bytes([0xFD, 0, 1]), "tx")
    assert obs.tx_signed_count == 0


def test_frame_observer_snapshot_shape():
    obs = FrameObserver()
    snap = obs.snapshot()
    assert set(snap.keys()) == {"tx_signed_count", "rx_signed_count", "last_signed_rx_at"}


# ──────────────────────────────────────────────────────────────
# Legacy config migration
# ──────────────────────────────────────────────────────────────

def test_legacy_mavlink_signing_block_ignored():
    """Old configs with mavlink.signing block must load without crashing.

    Dead SigningConfig scaffolding was removed from the agent. The
    MavlinkConfig `_drop_legacy_signing` validator strips the block so
    upgraded agents do not fail at config load time.
    """
    legacy = {
        "serial_port": "/dev/ttyACM0",
        "baud_rate": 57600,
        "system_id": 1,
        "component_id": 191,
        "signing": {"enabled": False, "key": "old-dead-scaffolding"},
    }
    cfg = MavlinkConfig.model_validate(legacy)
    assert cfg.baud_rate == 57600
    assert not hasattr(cfg, "signing")


def test_default_config_has_no_signing_field():
    """Dead SigningConfig was deleted. MavlinkConfig has no `signing` attr."""
    cfg = ADOSConfig()
    assert not hasattr(cfg.mavlink, "signing")
