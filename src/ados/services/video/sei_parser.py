"""H.264 SEI parser for the ADOS air-side latency marker.

Pure-Python byte walker with no heavy dependencies (no PIL, no
GStreamer, no third-party libs) so any agent service can import it.
Mirror of the on-the-wire format produced by the native injector in
``crates/ados-video/src/sei.rs``.

Extracted from the LCD-side tap that used to own it, so headless services that need the
parser (e.g. the drone-side :class:`HeadlessSeiTap`) don't have to
pay the cost of loading PIL just to walk bytes.
"""

from __future__ import annotations

from collections.abc import Iterator
from typing import NamedTuple

# Mirror of ADOS_LATENCY_SEI_UUID in crates/ados-video/src/sei.rs. Kept as a literal so
# this module imports cheaply.
ADOS_LATENCY_SEI_UUID = bytes.fromhex("ad05140e9c2c4f6e8a31f0e5b7d4c8a2")
assert len(ADOS_LATENCY_SEI_UUID) == 16
# Mirror of WE_SEI_UUID in crates/ados-vision-engine/src/sei.rs: the frame
# identity a simulated camera stamps on every frame.
WE_FRAME_SEI_UUID = bytes.fromhex("5745454e47494e458000000000000001")


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
                    positions.append((i + 3, i))
                    i += 3
                    continue
                if (
                    i + 3 < n
                    and stream[i + 2] == 0
                    and stream[i + 3] == 1
                ):
                    positions.append((i + 4, i))
                    i += 4
                    continue
            i += 1
        for idx, (start, _sc) in enumerate(positions):
            # The previous NAL ends where the NEXT START CODE BEGINS, which is
            # why both offsets are tracked. Deriving the end from the payload
            # start alone (`next_start - 4`) assumes the 4-byte start code and
            # truncates by one byte whenever the next one is the 3-byte form,
            # which h264parse emits depending on upstream caps.
            end = positions[idx + 1][1] if idx + 1 < len(positions) else n
            # Trim Annex-B trailing_zero_8bits padding. Safe only because `end`
            # is exact: an rbsp always ends on a non-zero byte (it carries
            # rbsp_stop_one_bit, 0x80 for our markers), so a zero here is
            # padding. With `end` one byte low this loop ate a real trailing
            # 0x00 instead, failing the SEI's own length check and discarding
            # the marker -- losing the latency sample for any ns value whose low
            # byte is 0x00, about one timestamp in 250, silently.
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

    Exact inverse of ``emulation_prevent`` in ``crates/ados-video/src/sei.rs``. Idempotent on
    streams that have no escape bytes (a no-op).

    Per H.264 §7.3.1 the decoder discards the ``0x03`` of a ``00 00 03``
    triple and then resumes scanning at the byte that followed it. This
    used to consume that byte unconditionally, which desynchronised the
    pair whenever an escape was immediately followed by another
    zero-run — the shape a conforming encoder emits for four or more
    consecutive zero bytes (``00 00 03 00 00 03 …``). Escaping and
    stripping are only exact inverses with the standard resume, and the
    inverse is what the injector's fix depends on.
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
        else:
            out.append(ebsp[i])
            i += 1
    return bytes(out)


def _iter_user_data(stream: bytes) -> Iterator[tuple[bytes, bytes]]:
    """Yield ``(uuid, body)`` of every ``user_data_unregistered`` SEI
    message (payload type 5) in ``stream``, in stream order."""
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
            if ptype == 5 and len(data) >= 16:
                yield data[:16], data[16:]


def parse_sei_latency_ns(stream: bytes) -> int | None:
    """Extract the air-side wall-clock-time-ns from a SEI marker.

    Returns the ns timestamp encoded by ``build_sei_nal`` in ``crates/ados-video/src/sei.rs``,
    or ``None`` if no matching SEI is present in the buffer.
    """
    for uuid, body in _iter_user_data(stream):
        if uuid == ADOS_LATENCY_SEI_UUID and len(body) >= 8:
            return int.from_bytes(body[:8], "big", signed=False)
    return None


class WeFrameId(NamedTuple):
    """A simulated camera frame's identity (``crates/ados-vision-engine/src/sei.rs``)."""

    seq: int
    sim_time_ns: int
    cam: int


def parse_we_frame_id(stream: bytes) -> WeFrameId | None:
    """The newest World Engine frame identity in ``stream``: the simulator's
    camera puts one in every frame, 24 bytes little-endian after the UUID
    (``seq`` u64, ``sim_time_ns`` u64, ``cam`` u32, reserved u32). ``None``
    when the buffer holds none (a real camera's stream)."""
    found: WeFrameId | None = None
    for uuid, body in _iter_user_data(stream):
        if uuid == WE_FRAME_SEI_UUID and len(body) >= 24:
            found = WeFrameId(
                seq=int.from_bytes(body[0:8], "little"),
                sim_time_ns=int.from_bytes(body[8:16], "little"),
                cam=int.from_bytes(body[16:20], "little"),
            )
    return found


__all__ = [
    "ADOS_LATENCY_SEI_UUID",
    "WE_FRAME_SEI_UUID",
    "WeFrameId",
    "parse_sei_latency_ns",
    "parse_we_frame_id",
]
