"""Parity tests for the Python MSP codec.

These pin the SAME golden byte vectors and CRC oracle values the Rust module
(``crates/ados-protocol/src/msp.rs``) pins, so the two codecs cannot drift on the
wire — a Python companion and a Rust plugin frame an MSP command identically.
"""

import pytest

from ados.plugins import msp

# ── CRC (independent oracle values, same as the Rust module) ─────────────────


def test_crc8_dvb_s2_known_values():
    assert msp.crc8_dvb_s2_buf(b"\x00") == 0x00
    assert msp.crc8_dvb_s2_buf(b"\x01") == 0xD5


# ── golden byte vectors ──────────────────────────────────────────────────────


def test_v1_status_request_golden():
    # MSP_STATUS (101), no payload: $ M < 00 65 crc, crc = 0x00 ^ 0x65.
    assert msp.encode_v1(101, b"") == bytes([ord("$"), ord("M"), ord("<"), 0x00, 0x65, 0x65])


def test_v1_with_payload_golden():
    crc = 2 ^ 200 ^ 0xE8 ^ 0x03
    assert msp.encode_v1(200, b"\xe8\x03") == bytes(
        [ord("$"), ord("M"), ord("<"), 0x02, 0xC8, 0xE8, 0x03, crc]
    )


def test_v1_rejects_oversize():
    assert msp.encode_v1(256, b"") is None  # cmd > 255
    assert msp.encode_v1(1, bytes(256)) is None  # payload > 255


def test_v2_status_request_golden():
    # CRC 0xCA from an INDEPENDENT DVB-S2 oracle, the same value the Rust golden
    # test pins — a symmetric encode/crc bug cannot pass both.
    assert msp.encode_v2(101, b"") == bytes(
        [ord("$"), ord("X"), ord("<"), 0x00, 0x65, 0x00, 0x00, 0x00, 0xCA]
    )


def test_set_raw_rc_v2_golden():
    # Two channels at 1500 (0xDC 0x05) each; cmd 200, size 4, CRC 0xB4.
    assert msp.set_raw_rc([1500, 1500], v2=True) == bytes(
        [ord("$"), ord("X"), ord("<"), 0x00, 0xC8, 0x00, 0x04, 0x00, 0xDC, 0x05, 0xDC, 0x05, 0xB4]
    )


# ── decode ───────────────────────────────────────────────────────────────────


@pytest.mark.parametrize("v2", [False, True])
def test_decode_round_trips(v2):
    raw = msp.set_raw_rc([1000, 1500, 2000, 1500], v2=v2)
    frame, consumed = msp.decode_frame(raw)
    assert consumed == len(raw)
    assert frame.cmd == msp.MSP_SET_RAW_RC
    assert frame.dir == msp.DIR_TO_FC
    assert frame.version == (2 if v2 else 1)
    chans = [int.from_bytes(frame.payload[i : i + 2], "little") for i in range(0, len(frame.payload), 2)]
    assert chans == [1000, 1500, 2000, 1500]


def test_decode_rejects_corrupt_crc():
    for v2 in (False, True):
        raw = bytearray(msp.set_raw_rc([1500], v2=v2))
        raw[-1] ^= 0xFF
        with pytest.raises(msp.MspBadCrc):
            msp.decode_frame(bytes(raw))


def test_decode_reports_short_and_bad_preamble():
    with pytest.raises(msp.MspTooShort):
        msp.decode_frame(b"$")
    with pytest.raises(msp.MspBadPreamble):
        msp.decode_frame(bytes([ord("$"), ord("Z"), ord("<"), 0, 0, 0]))
    with pytest.raises(msp.MspTooShort):
        # A v2 frame missing its payload+crc is short, not corrupt.
        msp.decode_frame(bytes([ord("$"), ord("X"), ord("<"), 0, 0xC8, 0, 4, 0]))


# ── stick scaling (matches the GCS + the Rust codec) ─────────────────────────


def test_stick_scaling_matches():
    assert msp.bipolar_to_pwm(0.0) == 1500
    assert msp.bipolar_to_pwm(1.0) == 2000
    assert msp.bipolar_to_pwm(-1.0) == 1000
    assert msp.bipolar_to_pwm(2.0) == 2000  # clamp
    assert msp.bipolar_to_pwm(-2.0) == 1000  # clamp
    assert msp.bipolar_to_pwm(0.5) == 1750
    # Throttle idles at 1000, never mid-stick.
    assert msp.throttle_to_pwm(0.0) == 1000
    assert msp.throttle_to_pwm(1.0) == 2000
    assert msp.throttle_to_pwm(0.5) == 1500
    assert msp.throttle_to_pwm(-0.1) == 1000  # clamp
    assert msp.throttle_to_pwm(1.1) == 2000  # clamp


def test_sticks_to_channels_aetr_order():
    # Centre + idle throttle are NOT zeros; AETR = [roll, pitch, throttle, yaw].
    assert msp.sticks_to_channels(0.0, 0.0, 0.0, 0.0) == [1500, 1500, 1000, 1500]
    assert msp.sticks_to_channels(1.0, -1.0, 1.0, 1.0) == [2000, 1000, 2000, 2000]


# ── reassembly ───────────────────────────────────────────────────────────────


def _frame() -> bytes:
    return msp.set_raw_rc([1500, 1500, 1000, 1500], v2=True)


def test_reassembler_frame_split_across_chunks():
    f = _frame()
    mid = len(f) // 2
    r = msp.MspReassembler()
    assert r.push(f[:mid]) == []
    assert r.pending == mid
    got = r.push(f[mid:])
    assert len(got) == 1 and got[0].cmd == 200


def test_reassembler_glued_frames_both_decode():
    r = msp.MspReassembler()
    assert len(r.push(_frame() + _frame())) == 2


def test_reassembler_resyncs_past_garbage_and_skips_corrupt():
    # Leading garbage.
    assert len(msp.MspReassembler().push(b"\xde\xad\xbe" + _frame())) == 1
    # A corrupt frame followed by a good one: only the good one surfaces.
    bad = bytearray(_frame())
    bad[len(bad) // 2] ^= 0xFF
    assert len(msp.MspReassembler().push(bytes(bad) + _frame())) == 1


def test_reassembler_preamble_less_stream_is_bounded():
    r = msp.MspReassembler()
    for _ in range(1000):
        assert r.push(bytes(256)) == []
    assert r.pending <= 256
