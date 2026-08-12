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

from ados.services.video.sei_injector import (
    ADOS_LATENCY_SEI_UUID,
    _emulation_prevent,
    build_sei_nal,
)
from ados.services.video.sei_parser import (
    _iter_nal_units,
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


def test_trailing_zero_padding_is_trimmed_off_the_nal_extent() -> None:
    """The fix must not cost the padding trim it sits next to.

    Annex-B allows `trailing_zero_8bits` between the end of an rbsp and the next
    start code. Those are padding and must come off; the payload's own trailing
    bytes must not. An rbsp always ends on a non-zero byte, which is what makes
    the distinction safe once the end offset is exact.

    Asserted on the NAL EXTENT, not on the parsed timestamp, because the
    timestamp cannot see this. The SEI walk stops at the declared payload size
    and breaks out on leftover bytes, so untrimmed padding inside the yielded
    payload changes nothing about the value returned. The earlier version of
    this test asserted only `parse_sei_latency_ns(padded) == ns`, and it passed
    with the trim loop deleted outright -- a test named for a mechanism it could
    not observe. What the trim actually controls is where the NAL ends, so that
    is what this reads.
    """
    ns = _REALISTIC_NS_BASE | 0xAB
    padded = build_sei_nal(ns) + b"\x00\x00" + THREE_BYTE + bytes([0x65, 0x88])

    # The value still round-trips, but that is not what is under test here.
    assert parse_sei_latency_ns(padded) == ns

    sei = [payload for nal_type, payload in _iter_nal_units(padded) if nal_type == 6]
    assert len(sei) == 1, "expected exactly one SEI NAL in the fixture"
    assert sei[0][-1] == 0x80, (
        "the SEI payload must end on its rbsp_stop_one_bit, not on Annex-B "
        f"padding -- got {sei[0][-1]:#04x}, so the trailing zeros were kept"
    )
    # And the padding is not merely at the end: it is absent entirely.
    assert not sei[0].endswith(b"\x00"), "trailing_zero_8bits survived the trim"


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


def test_the_two_uuid_constants_are_the_same_bytes() -> None:
    """Pins the wire contract across the injector/parser pair.

    Asserting only `len(...) == 16` cannot fail: both modules carry a
    module-level `assert len(ADOS_LATENCY_SEI_UUID) == 16`, so a changed length
    raises during collection and the test never runs. What is genuinely
    unguarded is the two constants DIVERGING — they are separate literals in
    separate files, and the injector's own comment says it keeps its copy so an
    air-side rig can import it without pulling the parser's dependencies. Two
    literals that must match is the same shape as the two same-named parsers
    that cost this project 236 dropped samples.
    """
    from ados.services.video import sei_parser as parser_mod

    assert parser_mod.ADOS_LATENCY_SEI_UUID == ADOS_LATENCY_SEI_UUID, (
        "the injector stamps a different UUID than the parser searches for, so "
        "no marker this agent writes would ever be found"
    )
    assert len(ADOS_LATENCY_SEI_UUID) == 16


def _rbsp_for(ns: int) -> bytes:
    """The unescaped SEI rbsp the injector builds for `ns`."""
    payload = ADOS_LATENCY_SEI_UUID + struct.pack(">Q", ns)
    return bytes([0x05, len(payload)]) + payload + b"\x80"


# Timestamps whose bytes contain a `00 00 0x` run, so escaping actually fires.
# Measured: the previous version of this test used five values off a realistic
# clock base, and EVERY one produced zero escape bytes — the UUID has no 00 00
# run and that base's penultimate byte is 0xE2 — so it exercised the stripper
# only on data with nothing to strip. It also compared just the first two bytes
# (the literal `05 18` header), which holds with the stripper replaced by the
# identity function. Both halves were untested by the test named for them.
#
# `0x0000010000000001` is deliberately excluded: it also drives the injector into
# emitting a literal `00 00 01`, the separate defect recorded as the xfail below.
# Keeping it out means this test measures the escaper/stripper inverse and not
# that bug.
_ESCAPING_NS = (
    0x1234_0000_0100_0000,
    0x18C6_0000_0200_00AB,
    0x0000_0000_0000_0003,
    0x18C6_7F00_0003_0080,
    0x18C6_7F5B_0000_0100,
)


def test_emulation_prevention_round_trips() -> None:
    """The stripper must be the exact inverse of the escaper for our payloads."""
    for ns in _ESCAPING_NS:
        raw = _rbsp_for(ns)
        escaped = _emulation_prevent(raw)
        assert len(escaped) > len(raw), (
            f"{ns:#018x} inserted no escape bytes, so this case proves nothing "
            "about emulation prevention"
        )
        assert _remove_emulation_prevention(escaped) == raw, (
            f"stripping is not the inverse of escaping for {ns:#018x}"
        )


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
