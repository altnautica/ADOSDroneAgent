"""H.264 SEI marker injector for over-the-air latency measurement.

Reads Annex-B H.264 from stdin, prepends a SEI NAL unit carrying a
wall-clock timestamp before each VCL slice, writes the modified
stream to stdout. Sits between two ffmpeg subprocesses inside the
wfb-tee bash pipeline:

    ffmpeg-input (RTSP -> stdout Annex-B)
        | python -m ados.services.video.sei_injector
        | ffmpeg-output (stdin -> RTP UDP 5600)

The receiver (LocalVideoTap.parse_sei_latency_ns) reads the same UUID
and computes ``time.time_ns() - air_ns`` to surface latency on the
LCD Video tab. Both ends rely on chrony / systemd-timesyncd to keep
wall clocks aligned within a few ms on the LAN.

One SEI per VCL slice ~= one per encoded frame at libx264 zerolatency
single-slice. Bandwidth cost: ~30 bytes per frame * 30 fps ~= 900 B/s,
negligible compared to a 4 Mbps video stream.
"""

from __future__ import annotations

import struct
import sys
import time
from typing import BinaryIO

# Mirror of LocalVideoTap.ADOS_LATENCY_SEI_UUID. Kept as a literal here
# so an air-side rig can import this module without pulling in PIL /
# gstreamer (which local_tap depends on).
ADOS_LATENCY_SEI_UUID = bytes.fromhex("ad05140e9c2c4f6e8a31f0e5b7d4c8a2")
assert len(ADOS_LATENCY_SEI_UUID) == 16

# Annex-B start code: 4-byte form (used at frame boundaries; the
# 3-byte form 00 00 01 is also valid mid-stream).
_START_CODE = b"\x00\x00\x00\x01"

# H.264 NAL unit header byte for SEI: forbidden_zero_bit=0,
# nal_ref_idc=0, nal_unit_type=6.
_NAL_HEADER_SEI = 0x06

# H.264 SEI payload type for user_data_unregistered (carries our UUID
# + ns blob).
_SEI_PAYLOAD_TYPE_USER_DATA_UNREG = 0x05


def _emulation_prevent(rbsp: bytes) -> bytes:
    """Insert emulation prevention bytes per H.264 §7.4.1.1.

    The decoder treats `00 00 00`, `00 00 01`, `00 00 02`, and
    `00 00 03` as forbidden inside a NAL's RBSP. Replace any such
    pattern with `00 00 03 <XX>`. Without this, a SEI payload that
    happens to contain `00 00 01` would be mis-parsed as the start
    of the next NAL.
    """
    out = bytearray()
    i = 0
    n = len(rbsp)
    while i < n:
        if (
            i + 2 < n
            and rbsp[i] == 0
            and rbsp[i + 1] == 0
            and rbsp[i + 2] in (0, 1, 2, 3)
        ):
            out.append(0)
            out.append(0)
            out.append(3)
            out.append(rbsp[i + 2])
            i += 3
        else:
            out.append(rbsp[i])
            i += 1
    return bytes(out)


def build_sei_nal(timestamp_ns: int) -> bytes:
    """Construct an Annex-B-framed SEI NAL with the latency timestamp.

    Layout:
        00 00 00 01      (start code)
        06               (NAL header: SEI)
        05 18            (SEI payload type 5, size 24)
        <UUID 16 bytes>
        <timestamp_ns 8 bytes big-endian>
        80               (rbsp_trailing_bits)
    """
    payload = ADOS_LATENCY_SEI_UUID + struct.pack(">Q", timestamp_ns)
    sei_msg = bytes([_SEI_PAYLOAD_TYPE_USER_DATA_UNREG, len(payload)]) + payload
    rbsp_trailing = bytes([0x80])
    nal_rbsp = sei_msg + rbsp_trailing
    nal_byte = bytes([_NAL_HEADER_SEI])
    return _START_CODE + nal_byte + _emulation_prevent(nal_rbsp)


def is_vcl_nal_type(nal_byte: int) -> bool:
    """NAL types 1 (non-IDR slice) and 5 (IDR slice) are VCL slices.

    SEI markers are prepended before every VCL NAL so the receiver
    sees a fresh timestamp at access-unit cadence. Non-VCL NALs
    (SPS=7, PPS=8, AUD=9, SEI=6) are passed through unchanged.
    """
    return (nal_byte & 0x1F) in (1, 5)


def _find_start_code(buf: bytes, start: int) -> tuple[int, int] | None:
    """Find the next Annex-B start code at or after ``start``.

    Returns ``(offset, sc_len)`` where ``sc_len`` is 3 or 4. Returns
    None if no complete start code is found in the buffer.
    """
    n = len(buf)
    i = start
    while i < n:
        if i + 4 <= n and buf[i : i + 4] == b"\x00\x00\x00\x01":
            return (i, 4)
        if i + 3 <= n and buf[i : i + 3] == b"\x00\x00\x01":
            return (i, 3)
        i += 1
    return None


def inject_stream(
    reader: BinaryIO,
    writer: BinaryIO,
    *,
    chunk_size: int = 65536,
) -> None:
    """Stream Annex-B from reader to writer, prepending SEI before each VCL.

    Buffers a 4-byte tail across chunk boundaries so a start code
    spanning two reads is detected on the next iteration. Writes
    output synchronously after each chunk so latency added by the
    injector is bounded by chunk_size / link_bitrate (~16 ms at 4
    Mbps with a 64 KB chunk; tighter with smaller chunks).
    """
    buf = bytearray()
    while True:
        chunk = reader.read(chunk_size)
        if not chunk:
            if buf:
                writer.write(bytes(buf))
                writer.flush()
            return
        buf += chunk
        # Walk the buffer, emitting bytes and prepending SEI before
        # any VCL NAL. Keep the tail (last 4 bytes) uncommitted so a
        # partial start code at the boundary survives to the next
        # chunk.
        out = bytearray()
        commit_until = 0
        i = 0
        n = len(buf)
        while i < n:
            sc = _find_start_code(buf, i)
            if sc is None:
                break
            sc_offset, sc_len = sc
            nal_byte_idx = sc_offset + sc_len
            if nal_byte_idx >= n:
                # Start code without a NAL byte yet — wait for next
                # chunk before deciding whether to inject.
                break
            nal_byte = buf[nal_byte_idx]
            if is_vcl_nal_type(nal_byte):
                # Emit pending bytes up to but not including the
                # start code, inject SEI, then let the start code
                # flow through normally on the next iteration.
                out += buf[commit_until:sc_offset]
                out += build_sei_nal(time.time_ns())
                commit_until = sc_offset
            # Advance past the start code so the next iteration scans
            # from inside this NAL.
            i = nal_byte_idx + 1
        # Commit out plus pending bytes up to the keep-back tail.
        keep_back = min(4, n - commit_until)
        emit_until = n - keep_back
        if emit_until > commit_until:
            out += buf[commit_until:emit_until]
        if out:
            writer.write(bytes(out))
            writer.flush()
        # Trim the buffer to whatever bytes we haven't committed.
        buf = bytearray(buf[emit_until:])


def main() -> None:
    inject_stream(sys.stdin.buffer, sys.stdout.buffer)


if __name__ == "__main__":
    main()
