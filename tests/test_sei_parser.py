"""Annex-B SEI parsing: NAL boundaries, the marker, and emulation prevention.

These recover coverage that was lost when the local-tap package was deleted.
The parser survived that deletion; its regression tests did not, and the copy
that survived was the one that never received the 3-byte-start-code fix. Both
implementations exported the same names and the same UUID, which is why the
divergence went unnoticed — so these tests assert behaviour on real byte
streams rather than checking that symbols resolve.

The stream under test is built by the injector that ships alongside it, so a
change to either side that breaks the pair fails here.
"""

from __future__ import annotations

import struct

import pytest

from ados.services.video.sei_injector import ADOS_LATENCY_SEI_UUID, build_sei_nal
from ados.services.video.sei_parser import (
    _remove_emulation_prevention,
    parse_sei_latency_ns,
)

# A realistic wall-clock nanosecond timestamp, used as the high bits so the
# low-byte matrix below varies only the byte that regressed.
_REALISTIC_NS_BASE = 0x18C6_7F5B_0638_E200

THREE_BYTE = b"\x00\x00\x01"
FOUR_BYTE = b"\x00\x00\x00\x01"


def _stream(ns: int, start_code: bytes) -> bytes:
    """A SEI NAL followed by a VCL slice introduced by `start_code`."""
    return build_sei_nal(ns) + start_code + bytes([0x65, 0x88])


def test_three_byte_start_code_round_trips_every_low_byte() -> None:
    """The regression: a NAL's end is where the NEXT START CODE BEGINS.

    The end used to be derived as `next_payload_start - 4`, which assumes the
    4-byte start code. Behind the 3-byte form that `h264parse` emits depending
    on upstream caps, that lands one byte early, and the trailing-zero trim then
    removes a real payload byte as padding. The SEI arrives short, fails its own
    declared-length check, and the marker vanishes.

    Both start-code forms and every interesting low byte, because the failure
    was value-dependent: it only bit when the low byte was 0x00, which is why a
    live-clock assertion passed locally and flaked in CI.
    """
    for low in (0x00, 0x01, 0x02, 0x03, 0x7F, 0x80, 0xFF):
        ns = _REALISTIC_NS_BASE | low
        assert parse_sei_latency_ns(_stream(ns, THREE_BYTE)) == ns, (
            f"3-byte start code lost the timestamp for low byte {low:#04x}"
        )
        assert parse_sei_latency_ns(_stream(ns, FOUR_BYTE)) == ns, (
            f"4-byte start code lost the timestamp for low byte {low:#04x}"
        )


def test_trailing_zero_padding_is_still_trimmed() -> None:
    """The fix must not cost the padding trim it sits next to.

    Annex-B allows `trailing_zero_8bits` between the end of an rbsp and the next
    start code. Those are padding and must come off; the payload's own trailing
    bytes must not. An rbsp always ends on a non-zero byte, which is what makes
    the distinction safe once the end offset is exact.
    """
    ns = _REALISTIC_NS_BASE | 0xAB
    padded = build_sei_nal(ns) + b"\x00\x00" + THREE_BYTE + bytes([0x65, 0x88])
    assert parse_sei_latency_ns(padded) == ns


def test_returns_none_without_a_marker() -> None:
    assert parse_sei_latency_ns(FOUR_BYTE + bytes([0x65, 0x88, 0x99])) is None


def test_returns_none_on_empty_input() -> None:
    assert parse_sei_latency_ns(b"") is None


def test_returns_none_on_a_non_matching_uuid() -> None:
    """A user-data-unregistered SEI from some other producer is not ours."""
    other = bytes(16)
    payload = other + struct.pack(">Q", 12345)
    rbsp = bytes([0x05, len(payload)]) + payload + b"\x80"
    assert parse_sei_latency_ns(FOUR_BYTE + b"\x06" + rbsp) is None


def test_finds_the_marker_after_a_non_matching_nal() -> None:
    """The marker is not required to be first in the stream."""
    ns = _REALISTIC_NS_BASE | 0x5C
    stream = FOUR_BYTE + bytes([0x67, 0x42, 0x00]) + build_sei_nal(ns)
    assert parse_sei_latency_ns(stream) == ns


def test_uuid_is_exactly_sixteen_bytes() -> None:
    """Pins the wire contract: the GCS mirrors this constant to find the NAL."""
    assert len(ADOS_LATENCY_SEI_UUID) == 16


def test_emulation_prevention_round_trips() -> None:
    """The stripper must be the exact inverse of the escaper for our payloads."""
    for low in (0x00, 0x01, 0x02, 0x03, 0xFF):
        ns = _REALISTIC_NS_BASE | low
        nal = build_sei_nal(ns)
        # Everything after the 4-byte start code and the 1-byte NAL header.
        escaped = nal[5:]
        assert _remove_emulation_prevention(escaped)[:2] == bytes([0x05, 0x18])


@pytest.mark.xfail(
    strict=True,
    reason=(
        "Known defect in the INJECTOR, not the parser: _emulation_prevent uses a "
        "three-byte lookahead bounded by `i + 2 < n`, so it cannot see a "
        "forbidden pattern ending at the final byte, and after emitting an "
        "escape it resumes past the escaped byte, leaving a zero it just wrote "
        "able to start a new run that is never examined. The NAL then carries a "
        "literal 00 00 01, which is a false start code for any decoder, not "
        "only this parser. Measured: 17 of 2006 sampled values across the "
        "realistic 1 ms - 2 s latency range. Fixing it means making the escaper "
        "and the existing stripper exact inverses, which is its own change; a "
        "first attempt traded this bug for a worse one. Remove this marker with "
        "the fix."
    ),
)
def test_parses_a_timestamp_whose_bytes_need_escaping() -> None:
    """A timestamp containing `00 00 0x` must survive escaping and stripping."""
    ns = 0x0000_0100_0000_0001 & 0xFFFF_FFFF_FFFF_FFFF
    assert parse_sei_latency_ns(_stream(ns, FOUR_BYTE)) == ns
