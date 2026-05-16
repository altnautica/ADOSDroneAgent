"""MAVLink v2 frame assembly primitives shared by every encoder.

Pure functions only. The encoder layer is intentionally decoupled from
the runtime so any caller (plugin host, scripting tier, test harness) can
build a frame without instantiating a MAVLink connection object.

Wire format (MAVLink v2, unsigned):

    STX(0xFD) LEN INCOMPAT COMPAT SEQ SYSID COMPID MSGID(3 bytes LE)
    PAYLOAD(LEN bytes)
    CRC(2 bytes LE)

Empty-byte truncation: per the v2 spec, trailing zero bytes are stripped
from the payload before length is computed and the CRC is taken. The
length field reflects the truncated length. At least one payload byte is
always retained.
"""

from __future__ import annotations

import struct
from typing import Final

_STX_V2: Final[int] = 0xFD
_HEADER_FMT: Final[struct.Struct] = struct.Struct("<BBBBBBBHB")
_CRC_FMT: Final[struct.Struct] = struct.Struct("<H")


def _x25_crc(data: bytes) -> int:
    """X.25 CRC (CRC-16/MCRF4XX) used by MAVLink for frame integrity.

    Matches the reference algorithm in the MAVLink C library and in
    pymavlink. Caller passes the bytes-to-be-protected; this function
    returns the 16-bit accumulator value with the seed 0xFFFF baked in.
    """
    accum = 0xFFFF
    for b in data:
        tmp = b ^ (accum & 0xFF)
        tmp = (tmp ^ (tmp << 4)) & 0xFF
        accum = (accum >> 8) ^ (tmp << 8) ^ (tmp << 3) ^ (tmp >> 4)
    return accum & 0xFFFF


def _truncate(payload: bytes) -> bytes:
    """Strip trailing zero bytes from a payload (v2 empty-byte truncation)."""
    end = len(payload)
    while end > 1 and payload[end - 1] == 0:
        end -= 1
    return payload[:end]


def pack_v2(
    *,
    msg_id: int,
    crc_extra: int,
    payload: bytes,
    sys_id: int,
    comp_id: int,
    seq: int,
    incompat_flags: int = 0,
    compat_flags: int = 0,
) -> bytes:
    """Assemble a complete MAVLink v2 frame around an already-packed payload.

    The payload must be little-endian and already follow the dialect's
    wire-order rules (largest fields first, then declaration order for
    fields of equal size, with arrays kept contiguous).

    Returns the full on-wire frame including STX and the trailing CRC.
    """
    if not 0 <= msg_id < (1 << 24):
        raise ValueError(f"msg_id out of MAVLink v2 range: {msg_id}")
    if not 0 <= sys_id <= 0xFF:
        raise ValueError(f"sys_id must fit in uint8: {sys_id}")
    if not 0 <= comp_id <= 0xFF:
        raise ValueError(f"comp_id must fit in uint8: {comp_id}")

    trimmed = _truncate(payload)
    header = _HEADER_FMT.pack(
        _STX_V2,
        len(trimmed),
        incompat_flags & 0xFF,
        compat_flags & 0xFF,
        seq & 0xFF,
        sys_id,
        comp_id,
        msg_id & 0xFFFF,
        (msg_id >> 16) & 0xFF,
    )
    # CRC is taken over LEN..end-of-payload, then accumulated with
    # crc_extra (the dialect-defined fingerprint byte for this msg id).
    crc = _x25_crc(header[1:] + trimmed)
    crc = _x25_crc_one(crc, crc_extra)
    return header + trimmed + _CRC_FMT.pack(crc)


def _x25_crc_one(accum: int, byte: int) -> int:
    """Continue an X.25 CRC accumulator with one additional byte."""
    tmp = (byte & 0xFF) ^ (accum & 0xFF)
    tmp = (tmp ^ (tmp << 4)) & 0xFF
    return ((accum >> 8) ^ (tmp << 8) ^ (tmp << 3) ^ (tmp >> 4)) & 0xFFFF
