# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Round-trip tests for the inference sidecar wire codec and server.

Pure Python: no rknn-toolkit-lite2, tensorrt, pycuda, or numpy import is needed
to pass. Inference is mocked so the transport and the response shape are what
get exercised. The response field names are asserted to match the Rust
``ados_protocol::framebus`` ``Detection`` / ``BoundingBox`` contract exactly.
"""

from __future__ import annotations

import asyncio
import struct

import msgpack
import pytest

from ados.services.vision import sidecar_protocol as proto
from ados.services.vision.sidecar_protocol import (
    InferRequest,
    LoadModelRequest,
    SidecarServer,
)

# ---------------------------------------------------------------------------
# Request dataclass round-trips
# ---------------------------------------------------------------------------


def test_load_model_request_round_trip():
    req = LoadModelRequest(
        model_id="com.example.weeds",
        path="/opt/ados/models/vision/weeds.rknn",
        input_w=640,
        input_h=640,
        format="rgb24",
        class_labels=["weed", "crop"],
    )
    decoded = LoadModelRequest.from_dict(req.to_dict())
    assert decoded == req
    assert req.to_dict()["op"] == proto.OP_LOAD_MODEL


def test_infer_request_round_trip_preserves_raw_bytes():
    frame = bytes(range(256)) * 4  # 1024 raw bytes, distinct from a string
    req = InferRequest(
        model_id="com.example.weeds",
        frame=frame,
        width=640,
        height=480,
        format="rgb24",
    )
    raw = req.to_dict()
    assert raw["op"] == proto.OP_INFER
    decoded = InferRequest.from_dict(raw)
    assert decoded.frame == frame
    assert isinstance(decoded.frame, bytes)
    assert decoded == req


def test_parse_request_dispatches_by_op():
    load = proto.parse_request(
        LoadModelRequest("m", "/p.rknn", 1, 1, "nv12").to_dict()
    )
    assert isinstance(load, LoadModelRequest)
    infer = proto.parse_request(
        InferRequest("m", b"\x00", 2, 2, "nv12").to_dict()
    )
    assert isinstance(infer, InferRequest)


def test_parse_request_rejects_unknown_op():
    with pytest.raises(proto.ProtocolError):
        proto.parse_request({"op": "explode"})


def test_infer_request_rejects_non_bytes_frame():
    with pytest.raises(proto.ProtocolError):
        InferRequest.from_dict(
            {"model_id": "m", "frame": "not-bytes", "width": 1, "height": 1, "format": "rgb24"}
        )


def test_infer_request_accepts_bytearray_and_memoryview():
    payload = bytearray(b"\x01\x02\x03\x04")
    decoded = InferRequest.from_dict(
        {"model_id": "m", "frame": payload, "width": 1, "height": 1, "format": "rgb24"}
    )
    assert decoded.frame == bytes(payload)
    assert isinstance(decoded.frame, bytes)


# ---------------------------------------------------------------------------
# Detection response shape matches the Rust contract
# ---------------------------------------------------------------------------


def test_detection_dict_matches_rust_field_names():
    det = proto.detection_dict(
        x=10.0,
        y=20.0,
        width=30.0,
        height=40.0,
        class_label="person",
        confidence=0.91,
        track_id=7,
    )
    # BoundingBox fields, exactly as the Rust struct names them.
    assert set(det["bbox"].keys()) == {"x", "y", "width", "height"}
    # Detection fields, exactly as the Rust struct names them.
    assert set(det.keys()) == {"bbox", "class_label", "confidence", "track_id"}
    assert det["bbox"]["x"] == 10.0
    assert det["bbox"]["width"] == 30.0
    assert det["class_label"] == "person"
    assert det["confidence"] == pytest.approx(0.91)
    assert det["track_id"] == 7


def test_detection_dict_defaults_track_id_to_none():
    det = proto.detection_dict(
        x=0.0, y=0.0, width=1.0, height=1.0, class_label="x", confidence=0.5
    )
    assert det["track_id"] is None


def test_ok_response_defaults_detections_to_empty_list():
    resp = proto.ok_response()
    assert resp == {"status": proto.STATUS_OK, "detections": []}


def test_error_response_carries_message():
    resp = proto.error_response("model not loaded: m")
    assert resp["status"] == proto.STATUS_ERROR
    assert resp["error"] == "model not loaded: m"


# ---------------------------------------------------------------------------
# Frame codec
# ---------------------------------------------------------------------------


def test_encode_message_uses_four_byte_be_length_prefix():
    payload = {"op": proto.OP_INFER, "model_id": "m"}
    frame = proto.encode_message(payload)
    (length,) = struct.unpack("!I", frame[: proto.HEADER_SIZE])
    assert length == len(frame) - proto.HEADER_SIZE
    body = frame[proto.HEADER_SIZE :]
    assert msgpack.unpackb(body, raw=False) == payload


def test_encode_message_keeps_bytes_distinct_from_str():
    # use_bin_type=True must keep the raw frame as msgpack bin, not str, so the
    # Rust rmp_serde decoder reads it as the byte field it expects.
    frame = proto.encode_message(InferRequest("m", b"\xff\x00", 1, 1, "rgb24").to_dict())
    body = frame[proto.HEADER_SIZE :]
    decoded = msgpack.unpackb(body, raw=False)
    assert isinstance(decoded["frame"], bytes)
    assert decoded["frame"] == b"\xff\x00"


def test_decode_message_rejects_non_mapping():
    body = msgpack.packb([1, 2, 3], use_bin_type=True)
    with pytest.raises(proto.ProtocolError):
        proto.decode_message(body)


async def _pump(payload: dict) -> dict:
    """Round-trip one payload through write_message / read_message over a real
    in-process socket pair."""
    pair = await _connected_stream_pair()
    try:
        await proto.write_message(pair.writer, payload)
        return await proto.read_message(pair.reader)
    finally:
        await pair.close()


class _StreamPair:
    """A loopback link: bytes written to ``writer`` are read from ``reader``.

    Backed by a single ``socketpair`` so neither end is torn down by a server
    handler returning. Each raw socket is wrapped once; the unused half of each
    wrap is tracked only so it can be closed cleanly.
    """

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        extra_writer: asyncio.StreamWriter,
    ) -> None:
        self.reader = reader
        self.writer = writer
        self._extra_writer = extra_writer

    async def close(self) -> None:
        for w in (self.writer, self._extra_writer):
            w.close()
            try:
                await w.wait_closed()
            except (ConnectionResetError, BrokenPipeError, OSError):
                pass


async def _connected_stream_pair() -> _StreamPair:
    import socket

    s_left, s_right = socket.socketpair()
    # The reader end: wrap s_left, keep its reader, drop its writer.
    reader, drop_writer = await asyncio.open_unix_connection(sock=s_left)
    # The writer end: wrap s_right, keep its writer, drop its reader.
    _drop_reader, writer = await asyncio.open_unix_connection(sock=s_right)
    return _StreamPair(reader, writer, drop_writer)


@pytest.mark.asyncio
async def test_stream_round_trip_load_request():
    req = LoadModelRequest("m", "/p.rknn", 320, 320, "rgb24", ["a", "b"]).to_dict()
    out = await _pump(req)
    assert LoadModelRequest.from_dict(out) == LoadModelRequest.from_dict(req)


@pytest.mark.asyncio
async def test_stream_round_trip_infer_response_with_detections():
    resp = proto.ok_response(
        [
            proto.detection_dict(
                x=1.0, y=2.0, width=3.0, height=4.0,
                class_label="dog", confidence=0.8, track_id=3,
            )
        ]
    )
    out = await _pump(resp)
    assert out["status"] == proto.STATUS_OK
    det = out["detections"][0]
    assert det["bbox"] == {"x": 1.0, "y": 2.0, "width": 3.0, "height": 4.0}
    assert det["class_label"] == "dog"
    assert det["confidence"] == pytest.approx(0.8)
    assert det["track_id"] == 3


@pytest.mark.asyncio
async def test_read_message_rejects_zero_length_frame():
    pair = await _connected_stream_pair()
    pair.writer.write((0).to_bytes(proto.HEADER_SIZE, "big"))
    await pair.writer.drain()
    with pytest.raises(proto.ProtocolError):
        await proto.read_message(pair.reader)
    await pair.close()


@pytest.mark.asyncio
async def test_read_message_returns_none_on_clean_eof():
    pair = await _connected_stream_pair()
    pair.writer.close()
    try:
        await pair.writer.wait_closed()
    except (ConnectionResetError, BrokenPipeError, OSError):
        pass
    assert await proto.read_message(pair.reader) is None


# ---------------------------------------------------------------------------
# Server end-to-end with a mock backend (no model, no numpy)
# ---------------------------------------------------------------------------


class _MockBackend:
    """A backend that records calls and returns canned protocol responses."""

    def __init__(self) -> None:
        self.loaded: list[str] = []
        self.inferred: list[str] = []

    def load_model(self, req: LoadModelRequest) -> dict:
        self.loaded.append(req.model_id)
        return proto.ok_response()

    def infer(self, req: InferRequest) -> dict:
        self.inferred.append(req.model_id)
        # One canned detection so the wire shape is exercised end to end.
        return proto.ok_response(
            [
                proto.detection_dict(
                    x=5.0, y=6.0, width=7.0, height=8.0,
                    class_label="cone", confidence=0.77,
                )
            ]
        )


@pytest.mark.asyncio
async def test_server_handles_load_then_infer(tmp_path):
    sock_path = str(tmp_path / "vision-mock.sock")
    backend = _MockBackend()
    server = SidecarServer(sock_path, backend, _SilentLog())
    serve_task = asyncio.create_task(server.serve_forever())

    # Wait for the socket to appear.
    for _ in range(200):
        if (tmp_path / "vision-mock.sock").exists():
            break
        await asyncio.sleep(0.01)

    reader, writer = await asyncio.open_unix_connection(path=sock_path)

    await proto.write_message(
        writer, LoadModelRequest("m1", "/p.rknn", 320, 320, "rgb24", ["cone"]).to_dict()
    )
    load_resp = await proto.read_message(reader)
    assert load_resp["status"] == proto.STATUS_OK

    await proto.write_message(
        writer, InferRequest("m1", b"\x00\x01\x02", 320, 320, "rgb24").to_dict()
    )
    infer_resp = await proto.read_message(reader)
    assert infer_resp["status"] == proto.STATUS_OK
    assert infer_resp["detections"][0]["class_label"] == "cone"
    assert infer_resp["detections"][0]["bbox"]["width"] == 7.0

    assert backend.loaded == ["m1"]
    assert backend.inferred == ["m1"]

    writer.close()
    await writer.wait_closed()
    await _stop_server(serve_task)


@pytest.mark.asyncio
async def test_server_returns_error_for_unknown_op(tmp_path):
    sock_path = str(tmp_path / "vision-err.sock")
    server = SidecarServer(sock_path, _MockBackend(), _SilentLog())
    serve_task = asyncio.create_task(server.serve_forever())

    for _ in range(200):
        if (tmp_path / "vision-err.sock").exists():
            break
        await asyncio.sleep(0.01)

    reader, writer = await asyncio.open_unix_connection(path=sock_path)
    await proto.write_message(writer, {"op": "explode", "model_id": "m"})
    resp = await proto.read_message(reader)
    assert resp["status"] == proto.STATUS_ERROR
    assert "explode" in resp["error"]

    writer.close()
    await writer.wait_closed()
    await _stop_server(serve_task)


async def _stop_server(serve_task: asyncio.Task) -> None:
    """Cancel a serving task and wait for it to wind down.

    ``serve_forever`` handles cancellation gracefully (it unlinks the socket
    and returns), so the task may either finish normally or re-raise the
    cancellation; both are accepted here.
    """
    serve_task.cancel()
    try:
        await serve_task
    except asyncio.CancelledError:
        pass


class _SilentLog:
    """A no-op logger stand-in matching the structlog call surface used."""

    def info(self, *args, **kwargs) -> None: ...
    def warning(self, *args, **kwargs) -> None: ...
    def error(self, *args, **kwargs) -> None: ...
