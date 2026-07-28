"""Byte-level round-trip tests for the AirPipeline SEI pad probe.

The pad probe splices a SEI NAL in front of the first VCL slice in an
access unit. The receiver-side parser (LocalVideoTap.parse_sei_latency_ns)
walks the same byte stream looking for the UUID and ns blob. If the
encoder and decoder agree on the byte layout, the parser round-trips.

These tests exercise the splice helper directly so we don't have to
spin up GStreamer in CI. The helper is the part of the probe that
actually owns the byte mutation; the rest is GstBuffer plumbing.
"""

from __future__ import annotations

import time

from ados.services.video import air_pipeline as ap
from ados.services.video import local_tap as lt


def _make_minimal_pipeline_for_probe() -> ap.AirPipeline:
    """Construct an AirPipeline without calling start().

    The splice helper ``_inject_sei_into_au`` is a pure method on the
    class with no dependency on PyGObject state, so we can instantiate
    a minimal object and exercise it directly.
    """
    from ados.core.config import VideoConfig

    cfg = VideoConfig()
    return ap.AirPipeline(
        video_config=cfg,
        camera=None,
        board_soc="BCM2711",
        board_hw_codecs=["h264_enc"],
        cloud_relay_enabled=False,
        sei_latency_enabled=True,
    )


def test_inject_sei_into_au_prefixes_sei_before_idr():
    pipe = _make_minimal_pipeline_for_probe()
    # IDR slice (NAL type 5) preceded by an Annex-B 4-byte start code.
    idr = b"\x00\x00\x00\x01" + bytes([0x65, 0x88, 0x80, 0x10])
    result = pipe._inject_sei_into_au(idr)
    assert result is not idr  # New bytes object on inject
    # The SEI sits between the original head (nothing here) and the IDR.
    parsed = lt.parse_sei_latency_ns(result)
    assert parsed is not None


def test_inject_sei_into_au_prefixes_sei_before_non_idr():
    pipe = _make_minimal_pipeline_for_probe()
    # Non-IDR slice (NAL type 1).
    slice_nal = b"\x00\x00\x00\x01" + bytes([0x41, 0x9A, 0xCC])
    result = pipe._inject_sei_into_au(slice_nal)
    parsed = lt.parse_sei_latency_ns(result)
    assert parsed is not None


def test_inject_sei_into_au_skips_non_vcl_buffer():
    pipe = _make_minimal_pipeline_for_probe()
    # SPS NAL only (NAL type 7), no VCL.
    sps = b"\x00\x00\x00\x01" + bytes([0x67, 0x42, 0x80, 0x1E])
    result = pipe._inject_sei_into_au(sps)
    # No splice => same reference returned.
    assert result is sps


def test_inject_sei_into_au_ns_value_round_trips_through_parser():
    pipe = _make_minimal_pipeline_for_probe()
    idr = b"\x00\x00\x00\x01" + bytes([0x65, 0x88])
    before = time.time_ns()
    result = pipe._inject_sei_into_au(idr)
    after = time.time_ns()
    parsed = lt.parse_sei_latency_ns(result)
    assert parsed is not None
    # The encoder stamps time.time_ns() at the moment of injection;
    # parsed value must sit within [before, after] inclusive.
    assert before <= parsed <= after


def test_inject_sei_with_3_byte_start_code():
    """Annex-B 3-byte start code (00 00 01) is also valid.

    h264parse can emit either 3- or 4-byte start codes depending on
    upstream caps. The probe must splice correctly on both.
    """
    pipe = _make_minimal_pipeline_for_probe()
    idr = b"\x00\x00\x01" + bytes([0x65, 0x88])
    result = pipe._inject_sei_into_au(idr)
    parsed = lt.parse_sei_latency_ns(result)
    assert parsed is not None


def test_three_byte_start_code_round_trips_a_timestamp_ending_in_a_zero_byte():
    """Regression: a ns value whose LOW BYTE is 0x00, behind a 3-byte start code.

    ``_iter_nal_units`` computed the previous NAL's end as
    ``next_payload_start - 4``, which assumes every start code is the 4-byte
    form. Behind a 3-byte start code that landed one byte early, and the
    ``trailing_zero_8bits`` trim then removed a legitimate trailing 0x00 payload
    byte as if it were padding. The SEI arrived two bytes short, failed its own
    declared-length check and was discarded, so the marker silently vanished for
    roughly one timestamp in 250 — about 0.4% of frames, or every frame on an
    encoder that always emits 3-byte start codes.

    It only ever failed for some clock values, so the original assertion
    (live clock, ``is not None``) passed locally and flaked in CI. This pins the
    exact class of value instead of sampling the clock and hoping.
    """
    pipe = _make_minimal_pipeline_for_probe()
    real = time.time_ns
    import time as _time

    # Every low byte, both start-code forms. 0x00 is the case that regressed;
    # the rest guard against a fix that trades one boundary for another.
    for low in (0x00, 0x01, 0x02, 0x03, 0x7F, 0x80, 0xFF):
        target_ns = 0x18C6_7F5B_0638_E200 | low
        try:
            _time.time_ns = lambda ns=target_ns: ns  # type: ignore[assignment]
            three = pipe._inject_sei_into_au(b"\x00\x00\x01" + bytes([0x65, 0x88]))
            four = pipe._inject_sei_into_au(
                b"\x00\x00\x00\x01" + bytes([0x65, 0x88])
            )
        finally:
            _time.time_ns = real  # type: ignore[assignment]
        assert lt.parse_sei_latency_ns(three) == target_ns, (
            f"3-byte start code lost the timestamp for low byte {low:#04x}"
        )
        assert lt.parse_sei_latency_ns(four) == target_ns, (
            f"4-byte start code lost the timestamp for low byte {low:#04x}"
        )


def test_annexb_trailing_zero_padding_is_still_trimmed():
    """The zero-trim must keep working for real `trailing_zero_8bits` padding.

    The fix above makes the NAL end exact, which is what makes trimming safe:
    an rbsp always ends on a non-zero byte because it carries
    `rbsp_stop_one_bit`, so a zero here is padding. This pins that the trim was
    narrowed rather than removed — a parser that stopped trimming would read
    padding as payload and overrun the SEI's declared length.
    """
    from ados.services.video.sei_injector import build_sei_nal

    target_ns = 0x0000_0001_0000_1234
    sei = build_sei_nal(target_ns)
    # rbsp, then two bytes of Annex-B padding, then a 3-byte start code.
    stream = sei + b"\x00\x00" + b"\x00\x00\x01" + bytes([0x65, 0x88])
    assert lt.parse_sei_latency_ns(stream) == target_ns


def test_inject_sei_handles_emulation_prevention_round_trip():
    """A ns value with 00 00 NN bytes must still round-trip.

    The encoder side inserts emulation-prevention 0x03 bytes; the
    receiver strips them in ``_remove_emulation_prevention``. This
    test exercises a ns value chosen to trigger an escape, locking
    the contract end-to-end.
    """
    pipe = _make_minimal_pipeline_for_probe()
    # Override time.time_ns to a value with an embedded 00 00 pattern
    # at byte positions where emulation-prevention will fire.
    target_ns = 0x1800_0002_3456_789A

    real = time.time_ns

    def fake_ns():
        return target_ns

    try:
        # Patch the function the splice helper calls.
        import time as _time
        _time.time_ns = fake_ns  # type: ignore[assignment]
        idr = b"\x00\x00\x00\x01" + bytes([0x65, 0x88])
        result = pipe._inject_sei_into_au(idr)
    finally:
        import time as _time
        _time.time_ns = real  # type: ignore[assignment]

    parsed = lt.parse_sei_latency_ns(result)
    assert parsed == target_ns
