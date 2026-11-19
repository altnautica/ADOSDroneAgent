"""The SEI tap on the simulator's clock: a simulated camera's frame carries the
World Engine frame identity SEI (`crates/ados-vision-engine/src/sei.rs`: UUID
then `seq`, `sim_time_ns`, `cam`, reserved, little-endian), and on a node
following the simulator's clock the tap reports a frame's age on ADOS time
from it. On wall time the latency SEI's stamp against the host clock stays the
reading."""

from __future__ import annotations

import struct
import time

from ados.sdk.clock import SimFeed
from ados.services.video.sei_parser import (
    WE_FRAME_SEI_UUID,
    WeFrameId,
    parse_sei_latency_ns,
    parse_we_frame_id,
)
from ados.services.video.sei_tap import HeadlessSeiTap

ADOS_LATENCY_SEI_UUID = bytes.fromhex("ad05140e9c2c4f6e8a31f0e5b7d4c8a2")
MS = 1_000_000


def _escape(rbsp: bytes) -> bytes:
    out = bytearray()
    zeros = 0
    for b in rbsp:
        if zeros >= 2 and b <= 0x03:
            out.append(0x03)
            zeros = 0
        out.append(b)
        zeros = zeros + 1 if b == 0x00 else 0
    return bytes(out)


def _sei(payload: bytes) -> bytes:
    rbsp = bytes([0x05, len(payload)]) + payload + b"\x80"
    return b"\x00\x00\x00\x01\x06" + _escape(rbsp)


def we_sei(seq: int, sim_time_ns: int, cam: int = 0) -> bytes:
    """The simulator camera's identity SEI, as its encoder writes it."""
    return _sei(WE_FRAME_SEI_UUID + struct.pack("<QQII", seq, sim_time_ns, cam, 0))


def latency_sei(ns: int) -> bytes:
    return _sei(ADOS_LATENCY_SEI_UUID + struct.pack(">Q", ns))


SLICE = b"\x00\x00\x00\x01\x65\x88\x84\x00\x10"


def _tick(seq: int, sim_ms: int) -> bytes:
    return struct.pack("<BBBBIQQd", 1, 1, 0, 0, seq, sim_ms * MS, (sim_ms + 20) * MS, 0.0)


def test_the_frame_identity_parses_and_the_newest_wins() -> None:
    stream = we_sei(7, 1_000 * MS, cam=1) + SLICE + we_sei(8, 1_100 * MS, cam=1) + SLICE
    assert parse_we_frame_id(stream) == WeFrameId(seq=8, sim_time_ns=1_100 * MS, cam=1)
    # The latency marker is a different message: neither reads the other.
    assert parse_sei_latency_ns(stream) is None
    assert parse_we_frame_id(latency_sei(123) + SLICE) is None
    # An identity whose bytes need escaping (zero runs) round-trips.
    assert parse_we_frame_id(we_sei(0, 0) + SLICE) == WeFrameId(0, 0, 0)


def test_on_the_sim_clock_latency_is_the_frames_age_on_ados_time() -> None:
    feed = SimFeed()
    tap = HeadlessSeiTap("rtsp://127.0.0.1:8554/main", sim_clock=feed)
    assert tap.clock == "sim"
    # Before the first tick there is no ADOS time: the marker is read, no sample.
    assert tap.take_sample(we_sei(1, 1_000 * MS) + SLICE)
    assert tap.payload()["samples"] == 0
    assert feed.take(_tick(1, 1_120), time.monotonic())
    # The wall-clock stamp is ignored at N x: only the sim age counts.
    assert tap.take_sample(latency_sei(time.time_ns() - 900 * MS) + we_sei(2, 1_100 * MS) + SLICE)
    p = tap.payload()
    assert p["latency_ms"] == 20.0
    assert p["samples"] == 1
    assert p["clock"] == "sim"
    assert p["sim_time_ns"] == 1_100 * MS
    # A frame ADOS time has not reached yet (inside one grant) is no sample.
    assert tap.take_sample(we_sei(3, 1_140 * MS) + SLICE)
    assert tap.payload()["samples"] == 0
    # No marker at all: the bytes are kept for the next read.
    assert not tap.take_sample(SLICE)


def test_on_wall_time_the_latency_stamp_stays_the_reading() -> None:
    tap = HeadlessSeiTap("rtsp://127.0.0.1:8554/main")
    assert tap.clock == "wall"
    assert tap.take_sample(we_sei(5, 1_000 * MS) + latency_sei(time.time_ns() - 40 * MS) + SLICE)
    p = tap.payload()
    assert p["clock"] == "wall"
    assert p["samples"] == 1
    assert 39.0 <= p["latency_ms"] < 1_000.0
    # The newest simulated frame is still named.
    assert p["sim_time_ns"] == 1_000 * MS
    # A simulated frame alone gives no wall reading.
    assert tap.take_sample(we_sei(6, 1_100 * MS) + SLICE)
    assert tap.payload()["samples"] == 0
