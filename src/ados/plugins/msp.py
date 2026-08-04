"""MSP (MultiWii Serial Protocol) v1 + v2 codec — the Python twin of
``crates/ados-protocol/src/msp.rs``.

The agent's router exposes a raw MSP byte pipe on ``/run/ados/msp.sock`` to a
Betaflight / iNav / KISS flight controller, but the Rust codec that builds and
parses MSP frames is only reachable from a Rust plugin. A Python companion loop
that flies an MSP FC (``MSP_SET_RAW_RC``) needs the same codec on this side, so
this module ports it byte-for-byte: the same two wire formats, the same CRC, and
the same stick scaling the GCS standardized. The parity tests pin the identical
golden vectors the Rust module pins, so the two halves cannot drift on the wire.

Two frame formats, both little-endian:

* **MSPv1** ``$M<``: ``'$' 'M' <dir> <size:u8> <cmd:u8> <payload...> <crc:u8>``,
  ``crc`` = XOR of ``size``, ``cmd``, and every payload byte.
* **MSPv2** ``$X<``: ``'$' 'X' <dir> <flag:u8> <cmd:u16 LE> <size:u16 LE>
  <payload...> <crc:u8>``, ``crc`` = CRC8/DVB-S2 (poly ``0xD5``) over ``flag``,
  the two command bytes, the two size bytes, and every payload byte.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

# MSP_SET_RAW_RC — write raw RC channel values (PWM microseconds) to the FC.
# Payload is N little-endian u16 channels. This is what flies an FC in a rate
# mode (ACRO), the FC treating the values as if they came from a receiver.
MSP_SET_RAW_RC = 200

DIR_TO_FC = ord("<")
DIR_FROM_FC = ord(">")
DIR_ERROR = ord("!")


class MspDecodeError(Exception):
    """Base for the reasons a buffer is not a single complete MSP frame."""


class MspTooShort(MspDecodeError):
    """Fewer bytes than a complete frame of its kind — wait for more."""


class MspBadPreamble(MspDecodeError):
    """The ``$M`` / ``$X`` preamble or version byte was not recognized."""


class MspBadCrc(MspDecodeError):
    """A complete frame whose trailing CRC did not match (corrupt)."""

    def __init__(self, expected: int, found: int) -> None:
        super().__init__(f"bad crc: expected {expected:#04x}, found {found:#04x}")
        self.expected = expected
        self.found = found


def crc8_dvb_s2(crc: int, byte: int) -> int:
    """Fold one byte into a CRC8/DVB-S2 running value (polynomial ``0xD5``)."""
    crc ^= byte
    for _ in range(8):
        if crc & 0x80:
            crc = ((crc << 1) ^ 0xD5) & 0xFF
        else:
            crc = (crc << 1) & 0xFF
    return crc


def crc8_dvb_s2_buf(data: bytes) -> int:
    """CRC8/DVB-S2 over a buffer, seeded at 0."""
    crc = 0
    for b in data:
        crc = crc8_dvb_s2(crc, b)
    return crc


def encode_v1(cmd: int, payload: bytes) -> bytes | None:
    """Encode an MSPv1 request ``$M<``. Returns ``None`` when ``cmd`` or the
    payload length exceeds a byte (a v2-only frame) rather than truncating."""
    if cmd > 0xFF or len(payload) > 0xFF:
        return None
    size = len(payload)
    out = bytearray([ord("$"), ord("M"), DIR_TO_FC, size, cmd])
    out.extend(payload)
    crc = size ^ cmd
    for b in payload:
        crc ^= b
    out.append(crc & 0xFF)
    return bytes(out)


def encode_v2(cmd: int, payload: bytes) -> bytes:
    """Encode an MSPv2 request ``$X<`` with ``flag = 0`` (the common case)."""
    return encode_v2_flagged(0, cmd, payload)


def encode_v2_flagged(flag: int, cmd: int, payload: bytes) -> bytes:
    """Encode an MSPv2 request ``$X<`` with an explicit flag byte."""
    size = len(payload)
    header = bytes([flag & 0xFF]) + cmd.to_bytes(2, "little") + size.to_bytes(2, "little")
    out = bytearray([ord("$"), ord("X"), DIR_TO_FC])
    out.extend(header)
    out.extend(payload)
    crc = crc8_dvb_s2_buf(header)
    for b in payload:
        crc = crc8_dvb_s2(crc, b)
    out.append(crc)
    return bytes(out)


@dataclass(frozen=True)
class MspFrame:
    """A decoded MSP frame: command id, direction/marker byte, payload, version."""

    cmd: int
    dir: int
    payload: bytes
    version: int


def decode_frame(buf: bytes) -> tuple[MspFrame, int]:
    """Decode exactly one MSP frame from the START of ``buf`` (v1 or v2,
    auto-detected). Returns ``(frame, consumed)`` so a streaming caller can
    advance. Raises :class:`MspTooShort` / :class:`MspBadPreamble` /
    :class:`MspBadCrc`; fails closed on a CRC mismatch (Rule 44)."""
    if len(buf) < 3:
        raise MspTooShort
    if buf[0] != ord("$"):
        raise MspBadPreamble
    if buf[1] == ord("M"):
        return _decode_v1(buf)
    if buf[1] == ord("X"):
        return _decode_v2(buf)
    raise MspBadPreamble


def _decode_v1(buf: bytes) -> tuple[MspFrame, int]:
    if len(buf) < 6:
        raise MspTooShort
    direction = buf[2]
    size = buf[3]
    cmd = buf[4]
    total = 6 + size
    if len(buf) < total:
        raise MspTooShort
    payload = bytes(buf[5 : 5 + size])
    crc = buf[3] ^ buf[4]
    for b in payload:
        crc ^= b
    found = buf[total - 1]
    if crc != found:
        raise MspBadCrc(crc, found)
    return MspFrame(cmd=cmd, dir=direction, payload=payload, version=1), total


def _decode_v2(buf: bytes) -> tuple[MspFrame, int]:
    if len(buf) < 9:
        raise MspTooShort
    direction = buf[2]
    cmd = int.from_bytes(buf[4:6], "little")
    size = int.from_bytes(buf[6:8], "little")
    total = 9 + size
    if len(buf) < total:
        raise MspTooShort
    payload = bytes(buf[8 : 8 + size])
    crc = crc8_dvb_s2_buf(bytes(buf[3:8]))  # flag + cmd + size
    for b in payload:
        crc = crc8_dvb_s2(crc, b)
    found = buf[total - 1]
    if crc != found:
        raise MspBadCrc(crc, found)
    return MspFrame(cmd=cmd, dir=direction, payload=payload, version=2), total


def set_raw_rc(channels: list[int], v2: bool = True) -> bytes | None:
    """Encode ``MSP_SET_RAW_RC`` carrying ``channels`` PWM values as little-endian
    ``u16``s. ``None`` only for a v1 encode with too many channels."""
    payload = b"".join(int(ch).to_bytes(2, "little") for ch in channels)
    if v2:
        return encode_v2(MSP_SET_RAW_RC, payload)
    return encode_v1(MSP_SET_RAW_RC, payload)


def _round_half_away(x: float) -> int:
    """Round half away from zero, matching Rust ``f32::round`` (Python's built-in
    ``round`` is banker's rounding, which would differ on a ``.5`` boundary)."""
    return int(math.floor(x + 0.5)) if x >= 0 else int(math.ceil(x - 0.5))


def bipolar_to_pwm(v: float) -> int:
    """Map a bipolar stick axis (roll/pitch/yaw), ``-1.0..=1.0``, to PWM
    ``1000..=2000`` with centre ``1500``. Out-of-range clamps."""
    clamped = max(-1.0, min(1.0, v))
    return _round_half_away(1500.0 + clamped * 500.0)


def throttle_to_pwm(t: float) -> int:
    """Map throttle ``0.0..=1.0`` to PWM ``1000..=2000`` with idle at ``1000``
    (NOT centre — a throttle at rest is idle, not mid-stick). Clamps."""
    clamped = max(0.0, min(1.0, t))
    return _round_half_away(1000.0 + clamped * 1000.0)


def sticks_to_channels(roll: float, pitch: float, yaw: float, throttle: float) -> list[int]:
    """Pack normalized sticks into AETR PWM channels ``[roll, pitch, throttle,
    yaw]`` — the codebase convention. Mirrors the Rust SDK ``send_sticks``."""
    return [
        bipolar_to_pwm(roll),
        bipolar_to_pwm(pitch),
        throttle_to_pwm(throttle),
        bipolar_to_pwm(yaw),
    ]


# A real MSP frame cannot exceed the v2 header + a u16 payload + CRC.
_MAX_MSP_FRAME = 9 + 0xFFFF + 1


class MspReassembler:
    """Accumulate a delivered MSP byte stream and emit whole frames — the Python
    twin of the SDK reassembler. A companion reading FC responses off the pipe
    feeds each delivered chunk to :meth:`push`; a frame straddling two chunks is
    reassembled rather than dropped, a corrupt frame is skipped, and a
    preamble-less stream is drained rather than hoarded."""

    def __init__(self) -> None:
        self._buf = bytearray()

    def push(self, chunk: bytes) -> list[MspFrame]:
        """Append ``chunk`` and return every complete frame now decodable."""
        self._buf.extend(chunk)
        out: list[MspFrame] = []
        while True:
            try:
                frame, consumed = decode_frame(bytes(self._buf))
            except MspTooShort:
                if len(self._buf) > _MAX_MSP_FRAME:
                    self._resync()
                    continue
                break
            except (MspBadPreamble, MspBadCrc):
                self._resync()
                continue
            out.append(frame)
            del self._buf[:consumed]
        return out

    @property
    def pending(self) -> int:
        """Bytes buffered but not yet a complete frame."""
        return len(self._buf)

    def _resync(self) -> None:
        """Advance to the next ``$``; clear the buffer if there is none. Always
        shrinks, so :meth:`push`'s loop cannot spin."""
        nxt = self._buf.find(b"$", 1)
        if nxt == -1:
            self._buf.clear()
        else:
            del self._buf[:nxt]
