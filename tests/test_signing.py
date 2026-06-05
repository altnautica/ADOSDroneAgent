"""Tests for the MAVLink signing helpers."""

from __future__ import annotations

import pytest
from pymavlink.dialects.v20 import common as mavlink2

from ados.core.config import ADOSConfig, MavlinkConfig
from ados.services.mavlink.signing import (
    FrameObserver,
    _fingerprint,
    detect_capability,
    disable_on_fc,
    enroll_fc,
    get_require,
    parse_key_hex,
    set_require,
)


def _decode_one(frame: bytes):
    """Decode a single packed MAVLink v2 frame back to a message object."""
    parser = mavlink2.MAVLink(None)
    parser.robust_parsing = True
    messages = parser.parse_buffer(frame)
    assert messages, "frame did not decode to a message"
    return messages[0]


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
# detect_capability (inputs from the router state IPC snapshot)
# ──────────────────────────────────────────────────────────────

def test_capability_fc_not_connected():
    result = detect_capability(False, 0, None)
    assert result["supported"] is False
    assert result["reason"] == "fc_not_connected"


def test_capability_rejects_px4():
    result = detect_capability(True, 12, None)  # MAV_AUTOPILOT_PX4
    assert result["supported"] is False
    assert result["reason"] == "firmware_px4_no_persistent_store"


def test_capability_rejects_unknown_autopilot():
    result = detect_capability(True, 99, None)
    assert result["supported"] is False
    assert result["reason"] == "firmware_not_supported"


def test_capability_ardupilot_without_signing_params():
    params = {"SYSID_THISMAV": 1.0, "BATT_CAPACITY": 5200.0}
    result = detect_capability(True, 3, params)  # MAV_AUTOPILOT_ARDUPILOTMEGA
    assert result["supported"] is False
    assert result["reason"] == "firmware_too_old"


def test_capability_ardupilot_with_signing_params():
    params = {"SYSID_THISMAV": 1.0, "SIGNING_REQUIRE": 0.0}
    result = detect_capability(True, 3, params)
    assert result["supported"] is True
    assert result["reason"] == "ok"
    assert result["firmware_name"] == "ArduPilot"
    assert result["signing_params_present"] is True


# ──────────────────────────────────────────────────────────────
# enroll_fc zeroize discipline (sends packed frames via the callback)
# ──────────────────────────────────────────────────────────────

@pytest.mark.asyncio
async def test_enroll_fc_zeroizes_key_on_success():
    sent: list[bytes] = []
    key = bytearray(b"A" * 32)
    result = await enroll_fc(sent.append, key)

    # Key buffer must be zeroized regardless of outcome.
    assert all(b == 0 for b in key), "enroll_fc must zeroize the key buffer"
    assert result["success"] is True
    assert len(result["key_id"]) == 8
    # SETUP_SIGNING is sent twice to survive a single-frame radio hiccup.
    assert len(sent) == 2
    msg = _decode_one(sent[0])
    assert msg.get_type() == "SETUP_SIGNING"


@pytest.mark.asyncio
async def test_enroll_fc_zeroizes_key_on_exception():
    def boom(_data: bytes) -> None:
        raise RuntimeError("radio dropped")

    key = bytearray(b"B" * 32)
    with pytest.raises(RuntimeError):
        await enroll_fc(boom, key)

    assert all(b == 0 for b in key), "enroll_fc must zeroize even on exception"


@pytest.mark.asyncio
async def test_enroll_fc_rejects_wrong_length_key():
    with pytest.raises(ValueError, match="32 bytes"):
        await enroll_fc(lambda _data: None, bytearray(16))


# ──────────────────────────────────────────────────────────────
# disable_on_fc and set_require
# ──────────────────────────────────────────────────────────────

def test_disable_on_fc_sends_zero_key():
    sent: list[bytes] = []
    result = disable_on_fc(sent.append)
    assert result["success"] is True

    msg = _decode_one(sent[0])
    assert msg.get_type() == "SETUP_SIGNING"
    assert bytes(msg.secret_key) == bytes(32)
    assert msg.initial_timestamp == 0


def test_set_require_writes_param():
    sent: list[bytes] = []
    result = set_require(sent.append, True)
    assert result["success"] is True
    assert result["require"] is True

    msg = _decode_one(sent[0])
    assert msg.get_type() == "PARAM_SET"
    assert msg.param_id.rstrip("\x00") == "SIGNING_REQUIRE"
    assert msg.param_value == 1.0


# ──────────────────────────────────────────────────────────────
# get_require (reads the cached param blob)
# ──────────────────────────────────────────────────────────────

def test_get_require_none_when_absent():
    assert get_require({})["require"] is None
    assert get_require(None)["require"] is None


def test_get_require_reads_cached_value():
    assert get_require({"SIGNING_REQUIRE": 1.0})["require"] is True
    assert get_require({"SIGNING_REQUIRE": 0.0})["require"] is False


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
