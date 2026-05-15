"""H.264 SEI parser for the ADOS air-side latency marker.

Pure-Python byte walker with no heavy dependencies (no PIL, no
GStreamer, no third-party libs) so any agent service can import it.
Mirror of the on-the-wire format produced by
:mod:`ados.services.video.sei_injector`.

Extracted from ``local_tap.py`` so headless services that need the
parser (e.g. the drone-side :class:`HeadlessSeiTap`) don't have to
pay the cost of loading PIL just to walk bytes.
"""

from __future__ import annotations

from typing import Iterator

# Mirror of sei_injector.ADOS_LATENCY_SEI_UUID. Kept as a literal so
# this module imports cheaply.
ADOS_LATENCY_SEI_UUID = bytes.fromhex("ad05140e9c2c4f6e8a31f0e5b7d4c8a2")
assert len(ADOS_LATENCY_SEI_UUID) == 16


def _iter_nal_units(stream: bytes) -> Iterator[tuple[int, bytes]]:
    """Yield ``(nal_type, payload)`` tuples from an Annex-B H.264 bytestream.

    Handles both Annex-B (start-code framed) and AVC (length-prefixed)
    NAL framing. Intentionally lenient: a malformed buffer yields
    nothing rather than raising, because the SEI parser is on the hot
    path and a cooked H.264 stream from a real encoder should not
    produce parser exceptions.
    """
    n = len(stream)
    if n < 4:
        return

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
            while end > start and stream[end - 1] == 0:
                end -= 1
            if end <= start:
                continue
            header = stream[start]
            nal_type = header & 0x1F
            payload = stream[start + 1 : end]
            yield nal_type, payload
        return

    # AVC length-prefixed framing.
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
    """Strip H.264 emulation-prevention escape bytes from an EBSP.

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
    """Extract the air-side wall-clock-time-ns from a SEI marker.

    Returns the ns timestamp encoded by ``sei_injector.build_sei_nal``,
    or ``None`` if no matching SEI is present in the buffer.
    """
    for nal_type, raw_payload in _iter_nal_units(stream):
        if nal_type != 6:
            continue
        if not raw_payload:
            continue
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
    return None


__all__ = [
    "ADOS_LATENCY_SEI_UUID",
    "parse_sei_latency_ns",
]
