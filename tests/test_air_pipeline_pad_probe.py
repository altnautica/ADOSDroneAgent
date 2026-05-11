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
