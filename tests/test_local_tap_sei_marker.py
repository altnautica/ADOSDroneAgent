"""Tests for the SEI latency-marker parser inside the local video tap.

The air-side encoder will eventually inject a SEI of type 5
(user_data_unregistered) carrying our 16-byte UUID followed by an 8-byte
big-endian uint64 of the encoder's wall-clock ``time.time_ns()``. The
ground-side parser extracts that timestamp from a synthetic Annex-B
H.264 buffer and yields the right delta against its own ``time_ns``
clock. When the marker is absent the parser must return None so the
metrics strip falls through to "—".

Wall-clock — not monotonic — because the air-side and ground-side run
on different hosts whose monotonic epochs are unrelated; both ends
rely on NTP to keep wall clocks aligned within a few ms.
"""

from __future__ import annotations

import time

from ados.services.video import local_tap as lt


def _build_sei_payload(uuid: bytes, ns_value: int) -> bytes:
    """Compose a SEI user-data-unregistered payload (type 5, payload-size 24)."""
    body = uuid + ns_value.to_bytes(8, "big", signed=False)
    # SEI payload format: <type=5> <size=24> <data>
    return bytes([5, len(body)]) + body


def _annexb_sei_nal(uuid: bytes, ns_value: int) -> bytes:
    """Wrap the SEI payload in an Annex-B framed NAL with type 6."""
    nal_header = bytes([0x06])  # forbidden=0, idc=0, type=6
    sei_payload = _build_sei_payload(uuid, ns_value)
    # 4-byte start code per Annex-B.
    return b"\x00\x00\x00\x01" + nal_header + sei_payload + b"\x80"


def _annexb_idr_nal() -> bytes:
    """A tiny stand-in IDR NAL so the parser sees more than just SEI."""
    return b"\x00\x00\x00\x01" + bytes([0x65, 0x88, 0x80, 0x10])


def test_parse_sei_returns_none_without_marker() -> None:
    stream = _annexb_idr_nal()
    assert lt.parse_sei_latency_ns(stream) is None


def test_parse_sei_returns_none_on_empty_input() -> None:
    assert lt.parse_sei_latency_ns(b"") is None


def test_parse_sei_returns_none_on_non_matching_uuid() -> None:
    other = bytes(16)
    stream = _annexb_sei_nal(other, 12345)
    assert lt.parse_sei_latency_ns(stream) is None


def test_parse_sei_extracts_ns_value() -> None:
    expected_ns = 0x0102_0304_0506_0708
    stream = _annexb_sei_nal(lt.ADOS_LATENCY_SEI_UUID, expected_ns)
    assert lt.parse_sei_latency_ns(stream) == expected_ns


def test_parse_sei_finds_marker_after_non_matching_nal() -> None:
    expected_ns = 999_999_999
    stream = _annexb_idr_nal() + _annexb_sei_nal(
        lt.ADOS_LATENCY_SEI_UUID, expected_ns,
    )
    assert lt.parse_sei_latency_ns(stream) == expected_ns


def test_uuid_is_exactly_sixteen_bytes() -> None:
    assert isinstance(lt.ADOS_LATENCY_SEI_UUID, bytes)
    assert len(lt.ADOS_LATENCY_SEI_UUID) == 16


def test_parse_sei_extracts_ns_with_emulation_prevention() -> None:
    """The receiver must de-escape H.264 emulation-prevention bytes.

    Real time.time_ns() values occasionally carry a 00 00 [0-3] byte
    pattern in the low bits; the encoder inserts a 0x03 escape per
    H.264 §7.4.1.1, and the receiver must remove it before reading
    the ns. Without de-escape the parser would read shifted bytes
    and the [0, 5000 ms] sanity bound would silently reject the
    sample. This test pins the round-trip property — use the actual
    encoder-side build_sei_nal to construct the wire format so any
    encoder/decoder drift trips this test.
    """
    from ados.services.video.sei_injector import build_sei_nal

    for ns in (
        # ns with 00 00 02 in the middle — escape required.
        0x1800_0002_3456_789A,
        # ns with 00 00 01 — would otherwise be misread as a NAL
        # start code.
        0x1800_0001_FFFF_FFFF,
        # ns with two consecutive escape triggers.
        0x0000_0001_0000_0002,
    ):
        nal = build_sei_nal(ns)
        recovered = lt.parse_sei_latency_ns(nal)
        assert recovered == ns, f"failed for ns=0x{ns:016x}"


def test_record_latency_sample_applies_ewma() -> None:
    tap = lt.LocalVideoTap()
    # First sample seeds the EWMA exactly.
    encoded_ns = time.time_ns() - 50_000_000  # 50 ms ago
    tap._record_latency_sample(encoded_ns)
    assert tap._latency_ewma is not None
    assert 40 <= tap._latency_ewma <= 60
    assert tap._latency_samples == 1


def test_record_latency_sample_rejects_negative_drift() -> None:
    tap = lt.LocalVideoTap()
    # Future encode timestamp (NTP drift across hosts mid-correction).
    encoded_ns = time.time_ns() + 100_000_000
    tap._record_latency_sample(encoded_ns)
    assert tap._latency_ewma is None
    assert tap._latency_samples == 0


def test_record_latency_sample_rejects_extreme_value() -> None:
    tap = lt.LocalVideoTap()
    # 10 seconds in the past — clearly bogus.
    encoded_ns = time.time_ns() - 10_000_000_000
    tap._record_latency_sample(encoded_ns)
    assert tap._latency_ewma is None
    assert tap._latency_samples == 0


def test_record_latency_sample_smooths_across_three_samples() -> None:
    tap = lt.LocalVideoTap()
    # Synthesize three samples at 30, 40, 50 ms by adjusting the encoded ns.
    for delay_ms in (30, 40, 50):
        encoded_ns = time.time_ns() - delay_ms * 1_000_000
        tap._record_latency_sample(encoded_ns)
    assert tap._latency_samples == 3
    assert tap._latency_ewma is not None
    # EWMA with alpha=0.2: starts at 30, then 0.2*40+0.8*30=32, then
    # 0.2*50+0.8*32=35.6. Allow ±5 ms slack for clock drift.
    assert 30.0 <= tap._latency_ewma <= 45.0


def test_stats_exposes_latency_ms_when_set() -> None:
    tap = lt.LocalVideoTap()
    encoded_ns = time.time_ns() - 25_000_000
    tap._record_latency_sample(encoded_ns)
    stats = tap.stats()
    assert "latency_ms" in stats
    assert isinstance(stats["latency_ms"], float)
    assert stats["latency_samples"] == 1


def test_stats_returns_none_latency_when_no_marker() -> None:
    tap = lt.LocalVideoTap()
    stats = tap.stats()
    assert stats["latency_ms"] is None
    assert stats["latency_samples"] == 0
