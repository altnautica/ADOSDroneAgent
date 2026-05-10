"""Tests for the air-side H.264 SEI injector.

Encoder-side counterpart of the ground-side parser at
``ados.services.video.local_tap.parse_sei_latency_ns``. The round-trip
test is the load-bearing assertion: feed
``build_sei_nal(N)`` output through the parser and verify ``N`` comes
out the other side. Confirms encoder/decoder byte-level agreement.
"""

from __future__ import annotations

import io
import struct

from ados.services.video.local_tap import (
    ADOS_LATENCY_SEI_UUID,
    parse_sei_latency_ns,
)
from ados.services.video.sei_injector import (
    _emulation_prevent,
    build_sei_nal,
    inject_stream,
    is_vcl_nal_type,
)


def _annexb(nal_byte: int, body: bytes = b"") -> bytes:
    """Helper: build a minimal Annex-B-framed NAL with the full
    NAL header byte (caller passes nal_ref_idc<<5 | nal_unit_type)."""
    return b"\x00\x00\x00\x01" + bytes([nal_byte & 0xFF]) + body


def test_emulation_prevent_escapes_zero_zero_nn() -> None:
    # Each of the four trigger patterns gets the 0x03 escape byte
    # inserted between the second 0x00 and the trigger byte.
    assert _emulation_prevent(b"\x00\x00\x00") == b"\x00\x00\x03\x00"
    assert _emulation_prevent(b"\x00\x00\x01") == b"\x00\x00\x03\x01"
    assert _emulation_prevent(b"\x00\x00\x02") == b"\x00\x00\x03\x02"
    assert _emulation_prevent(b"\x00\x00\x03") == b"\x00\x00\x03\x03"


def test_emulation_prevent_passthrough() -> None:
    # No 00 00 [0-3] patterns -> unchanged.
    raw = b"\x12\x34\x56\xff\x00\xff\x00\x05"
    assert _emulation_prevent(raw) == raw


def test_emulation_prevent_handles_consecutive_triggers() -> None:
    # 00 00 00 00 00 00 -> 00 00 03 00 00 00 03 00 00 (two escapes).
    src = b"\x00\x00\x00\x00\x00\x00"
    out = _emulation_prevent(src)
    # Verify we can de-escape it back to the original via the
    # receiver-side stripper (round-trip property).
    from ados.services.video.local_tap import _remove_emulation_prevention

    assert _remove_emulation_prevention(out) == src


def test_build_sei_nal_layout() -> None:
    # ns chosen so it has no internal 00 00 patterns; this isolates
    # the layout assertions from emulation prevention.
    ns = 0x0102_0304_0506_0708
    nal = build_sei_nal(ns)
    assert nal[:4] == b"\x00\x00\x00\x01"  # start code
    assert nal[4] == 0x06  # NAL header SEI
    assert nal[5] == 0x05  # payload type
    assert nal[6] == 0x18  # payload size = 24
    assert nal[7 : 7 + 16] == ADOS_LATENCY_SEI_UUID
    assert nal[23 : 23 + 8] == struct.pack(">Q", ns)
    assert nal[31] == 0x80  # rbsp_trailing_bits


def test_build_sei_nal_round_trip_with_parser() -> None:
    """Critical: encoder + decoder must agree on the byte format."""
    for ns in (
        0x0102_0304_0506_0708,
        # ns with a 00 00 pattern that triggers emulation prevention.
        0x1800_0002_3456_789A,
        # Edge case: 00 00 01 in the ns (would be misread as start
        # code without escape).
        0x1800_0001_FFFF_FFFF,
    ):
        nal = build_sei_nal(ns)
        recovered = parse_sei_latency_ns(nal)
        assert recovered == ns, f"round-trip failed for ns=0x{ns:016x}"


def test_is_vcl_nal_type() -> None:
    # Type 1 (P slice), 5 (IDR slice) are VCL.
    assert is_vcl_nal_type(0x01) is True
    assert is_vcl_nal_type(0x05) is True
    assert is_vcl_nal_type(0x21) is True  # nal_ref_idc=1, type=1
    assert is_vcl_nal_type(0x65) is True  # nal_ref_idc=3, type=5
    # Other types (SEI=6, SPS=7, PPS=8, AUD=9) are not.
    assert is_vcl_nal_type(0x06) is False
    assert is_vcl_nal_type(0x07) is False
    assert is_vcl_nal_type(0x08) is False
    assert is_vcl_nal_type(0x09) is False
    assert is_vcl_nal_type(0x00) is False


def _count_sei_markers(stream: bytes) -> int:
    """Count Annex-B SEI NALs (type 6) in a stream that carry our UUID."""
    count = 0
    i = 0
    n = len(stream)
    while i < n:
        if i + 4 <= n and stream[i : i + 4] == b"\x00\x00\x00\x01":
            sc_len = 4
        elif i + 3 <= n and stream[i : i + 3] == b"\x00\x00\x01":
            sc_len = 3
        else:
            i += 1
            continue
        nal_byte_idx = i + sc_len
        if nal_byte_idx >= n:
            break
        nal_type = stream[nal_byte_idx] & 0x1F
        if nal_type == 6:
            # Check our UUID appears within the next 32 bytes
            # (covers payload type, size, UUID start).
            window = stream[nal_byte_idx : nal_byte_idx + 64]
            if ADOS_LATENCY_SEI_UUID in window:
                count += 1
        i = nal_byte_idx + 1
    return count


def test_inject_stream_prepends_sei_before_each_vcl() -> None:
    # Stream: SPS (type 7) + PPS (type 8) + IDR slice (type 5) +
    # non-IDR slice (type 1). Two VCLs, expect two SEI markers in
    # output.
    stream_in = (
        _annexb(0x67, b"\x42\x00\x1f")  # SPS-ish stub
        + _annexb(0x68, b"\xeb\xef")  # PPS-ish stub
        + _annexb(0x65, b"\x88\x80\x10\x00")  # IDR slice
        + _annexb(0x41, b"\x9a\xbb\xcc\xdd")  # non-IDR slice
    )
    reader = io.BytesIO(stream_in)
    writer = io.BytesIO()
    inject_stream(reader, writer)
    out = writer.getvalue()
    assert _count_sei_markers(out) == 2
    # Original stream content is preserved (length grows only by SEI
    # NALs, no original NAL is dropped or modified).
    assert all(orig_nal in out for orig_nal in (
        b"\x00\x00\x00\x01\x67\x42\x00\x1f",
        b"\x00\x00\x00\x01\x68\xeb\xef",
        b"\x00\x00\x00\x01\x65\x88\x80\x10\x00",
        b"\x00\x00\x00\x01\x41\x9a\xbb\xcc\xdd",
    ))


def test_inject_stream_no_sei_when_no_vcl() -> None:
    # Stream of only non-VCL NALs -> no SEI injected.
    stream_in = (
        _annexb(0x67, b"\x42\x00\x1f")
        + _annexb(0x68, b"\xeb\xef")
        + _annexb(0x09, b"\x10")  # AUD
    )
    reader = io.BytesIO(stream_in)
    writer = io.BytesIO()
    inject_stream(reader, writer)
    out = writer.getvalue()
    assert _count_sei_markers(out) == 0
    assert out == stream_in


def test_inject_stream_handles_chunk_boundaries() -> None:
    # Same input fed in tiny chunks must yield identical output to a
    # one-shot read. A 7-byte chunk size is small enough to land inside
    # start codes, NAL headers, and SEI payloads.
    stream_in = (
        _annexb(0x67, b"\x42\x00\x1f")
        + _annexb(0x65, b"\x88\x80\x10\x00\x11\x22\x33\x44\x55\x66\x77")
        + _annexb(0x41, b"\xaa\xbb\xcc\xdd\xee\xff")
    )

    # One-shot reference output.
    reader_full = io.BytesIO(stream_in)
    writer_full = io.BytesIO()
    inject_stream(reader_full, writer_full, chunk_size=65536)
    full = writer_full.getvalue()

    # Chunked output. Same number of SEI markers, same VCL ordering
    # (we don't compare bytes byte-for-byte because each SEI carries
    # a fresh time.time_ns(), which differs between runs).
    reader_chunked = io.BytesIO(stream_in)
    writer_chunked = io.BytesIO()
    inject_stream(reader_chunked, writer_chunked, chunk_size=7)
    chunked = writer_chunked.getvalue()

    assert _count_sei_markers(full) == _count_sei_markers(chunked) == 2
    # Both outputs preserve the original NALs intact.
    for orig_nal in (
        b"\x00\x00\x00\x01\x67\x42\x00\x1f",
        b"\x00\x00\x00\x01\x65\x88\x80\x10\x00\x11\x22\x33\x44\x55\x66\x77",
        b"\x00\x00\x00\x01\x41\xaa\xbb\xcc\xdd\xee\xff",
    ):
        assert orig_nal in full
        assert orig_nal in chunked
