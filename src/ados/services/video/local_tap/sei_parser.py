"""SEI latency parser for the LCD video tap.

Pure-byte helpers used by :class:`LocalVideoTap`'s pad probe to recover
the air-side encoder's wall-clock ``time.time_ns()`` from a custom
H.264 user-data-unregistered SEI marker. Split out of the tap module
so the parser is unit-testable without spinning up GStreamer.
"""

from __future__ import annotations

from typing import Any

# 16-byte UUID prefix the air-side encoder will embed in a SEI of type
# 5 (user_data_unregistered) followed by an 8-byte big-endian uint64 of
# the encoder's wall-clock ``time.time_ns()``. Wall-clock — not
# monotonic — because the air-side and ground-side run on different
# hosts whose monotonic epochs are unrelated. Both ends rely on NTP /
# chrony / systemd-timesyncd to keep wall clocks within a few ms of
# each other, which is the standard assumption on a LAN-paired rig.
ADOS_LATENCY_SEI_UUID = bytes.fromhex("ad05140e9c2c4f6e8a31f0e5b7d4c8a2")
assert len(ADOS_LATENCY_SEI_UUID) == 16


def _iter_nal_units(stream: bytes) -> Any:
    """Yield ``(nal_type, payload)`` tuples from an Annex-B H.264 bytestream.

    Annex-B framing is the on-the-wire byte layout: each NAL unit is
    preceded by either ``00 00 00 01`` or ``00 00 01``. ``h264parse``
    can output either AVC (length-prefixed) or Annex-B; the agent's
    pipeline does not enforce a stream-format, so the parser handles
    both. AVC is detected by the absence of any start code: if no
    start-code prefix is found, we treat the input as length-prefixed
    NAL units with a 4-byte big-endian length header.

    The function is intentionally lenient — a malformed buffer yields
    nothing rather than raising, because the SEI parser is on the hot
    path and a cooked H.264 stream from a real encoder should not
    produce parser exceptions.
    """
    n = len(stream)
    if n < 4:
        return

    # Detect Annex-B: scan for the first 00 00 01 / 00 00 00 01.
    annexb_idx = -1
    i = 0
    while i + 2 < n:
        if stream[i] == 0 and stream[i + 1] == 0:
            if stream[i + 2] == 1:
                annexb_idx = i
                break
            if (
                i + 3 < n
                and stream[i + 2] == 0
                and stream[i + 3] == 1
            ):
                annexb_idx = i
                break
        i += 1

    if annexb_idx >= 0:
        # Annex-B: split on start codes.
        positions: list[int] = []
        i = annexb_idx
        while i + 2 < n:
            if stream[i] == 0 and stream[i + 1] == 0:
                if stream[i + 2] == 1:
                    positions.append(i + 3)
                    i += 3
                    continue
                if (
                    i + 3 < n
                    and stream[i + 2] == 0
                    and stream[i + 3] == 1
                ):
                    positions.append(i + 4)
                    i += 4
                    continue
            i += 1
        for idx, start in enumerate(positions):
            end = (
                positions[idx + 1] - 4
                if idx + 1 < len(positions)
                else n
            )
            # Trim a trailing 00 00 in case the next start code is
            # 00 00 01 (3-byte form).
            while end > start and stream[end - 1] == 0:
                end -= 1
            if end <= start:
                continue
            header = stream[start]
            nal_type = header & 0x1F
            payload = stream[start + 1 : end]
            yield nal_type, payload
        return

    # Length-prefixed (AVC). 4-byte big-endian length header per NAL.
    i = 0
    while i + 4 <= n:
        length = int.from_bytes(stream[i : i + 4], "big")
        i += 4
        if length <= 0 or i + length > n:
            return
        if length < 1:
            continue
        header = stream[i]
        nal_type = header & 0x1F
        payload = stream[i + 1 : i + length]
        yield nal_type, payload
        i += length


def _remove_emulation_prevention(ebsp: bytes) -> bytes:
    """Strip the H.264 emulation-prevention escape bytes from an EBSP.

    The encoder side inserts a ``0x03`` byte after any ``00 00`` pair
    when the next byte is in {0, 1, 2, 3}, per H.264 §7.4.1.1. Without
    removing those escapes here, a SEI payload that happened to carry
    a ``00 00`` pattern (typical for some ``time.time_ns()`` values
    that have two adjacent zero bytes) would be mis-parsed: the
    declared payload-size byte counts un-escaped bytes, but the raw
    EBSP contains one extra byte per escape, so ``data[16:24]`` would
    read shifted ns bytes.

    Inverse of ``sei_injector._emulation_prevent``. Idempotent on
    streams that have no escape bytes (a no-op).
    """
    out = bytearray()
    i = 0
    n = len(ebsp)
    while i < n:
        if (
            i + 2 < n
            and ebsp[i] == 0
            and ebsp[i + 1] == 0
            and ebsp[i + 2] == 3
        ):
            out.append(0)
            out.append(0)
            i += 3
            if i < n:
                out.append(ebsp[i])
                i += 1
        else:
            out.append(ebsp[i])
            i += 1
    return bytes(out)


def parse_sei_latency_ns(stream: bytes) -> int | None:
    """Extract the air-side encoder's wall-clock-time-ns from a SEI marker.

    Scans ``stream`` for an H.264 SEI NAL unit (NAL type 6) that
    contains a user-data-unregistered payload (payload type 5) whose
    16-byte UUID matches :data:`ADOS_LATENCY_SEI_UUID`. The next 8
    bytes are interpreted as a big-endian uint64 of the encoder's
    ``time.time_ns()`` at frame-encode time. Wall-clock so a comparison
    against the receiver's ``time.time_ns()`` produces meaningful
    glass-to-glass latency across hosts.

    Returns the encoded ns value, or ``None`` if no matching SEI is
    present in the buffer.
    """
    for nal_type, raw_payload in _iter_nal_units(stream):
        if nal_type != 6:
            continue
        if not raw_payload:
            continue
        # SEI message structure: <payload_type> <payload_size> <data>.
        # payload_type and payload_size are each ff-extended bytes per
        # the spec but in practice fit in one byte for our markers.
        # The raw payload coming off the wire is EBSP — strip H.264
        # emulation-prevention bytes before parsing so a ns value with
        # 00 00 patterns reads correctly.
        payload = _remove_emulation_prevention(raw_payload)
        idx = 0
        plen = len(payload)
        while idx < plen:
            ptype = 0
            while idx < plen and payload[idx] == 0xFF:
                ptype += 0xFF
                idx += 1
            if idx >= plen:
                break
            ptype += payload[idx]
            idx += 1
            psize = 0
            while idx < plen and payload[idx] == 0xFF:
                psize += 0xFF
                idx += 1
            if idx >= plen:
                break
            psize += payload[idx]
            idx += 1
            if idx + psize > plen:
                break
            data = payload[idx : idx + psize]
            idx += psize
            if (
                ptype == 5
                and len(data) >= 16 + 8
                and data[:16] == ADOS_LATENCY_SEI_UUID
            ):
                ns = int.from_bytes(data[16 : 16 + 8], "big", signed=False)
                return ns
        # Continue to the next NAL unit in case there are multiple.
    return None
