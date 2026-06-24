# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Wire codec for the NPU/TensorRT inference sidecar.

The Rust vision engine cannot host the proprietary Python-only inference
runtimes (rknn-toolkit-lite2 for RK3588/RK3582/RK3576, tensorrt for Jetson
Orin), so a small Python sidecar process owns the model and the inference call.
The engine reaches it over a Unix domain socket with the same framing every
other ADOS IPC socket uses: a 4-byte big-endian unsigned length prefix followed
by a msgpack body (see ``ados.core.ipc`` and the ``frame`` contract in the
``ados-protocol`` crate).

Two request operations cross the wire:

* ``load_model`` — open a model file and keep it resident, addressed by id.
* ``infer`` — run a resident model against one raw frame, returning detections.

Detection responses use the exact field names of the ``Detection`` and
``BoundingBox`` shapes the Rust client deserializes (``bbox`` with
``x``/``y``/``width``/``height``, plus ``class_label``, ``confidence``,
``track_id``), so a response decoded here round-trips into the Rust contract
without a translation layer.
"""

from __future__ import annotations

import asyncio
import contextlib
import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

import msgpack

# 4-byte big-endian length prefix, identical to the MAVLink and plugin sockets.
HEADER_SIZE = 4

# A raw RGB24 frame at 1280x720 is ~2.7 MiB; add msgpack overhead and round up.
# Frames larger than this should travel through the shared-memory ring instead
# of inline in a request, so this cap doubles as a misuse guard.
MAX_FRAME_BYTES = 8 * 1024 * 1024

# Request operations.
OP_LOAD_MODEL = "load_model"
OP_INFER = "infer"
OP_EMBED = "embed"

# Response statuses.
STATUS_OK = "ok"
STATUS_ERROR = "error"


class ProtocolError(Exception):
    """Raised on a malformed length prefix, an oversized frame, or a body that
    is not a msgpack mapping."""


@dataclass
class LoadModelRequest:
    """Ask the sidecar to load a model file and keep it resident under
    ``model_id``. ``format`` is the input pixel format the model expects, one of
    the lowercase :class:`ados_protocol::framebus::FrameFormat` names
    (``rgb24``, ``nv12``, ``yuv420p``)."""

    model_id: str
    path: str
    input_w: int
    input_h: int
    format: str
    # Class labels in output-index order. Carried so the sidecar can label
    # detections even when the caller (not the model file) owns the labels.
    class_labels: list[str] = field(default_factory=list)
    # Output-head layout for decoding: "yolov8" (the transposed [1, 4+nc,
    # anchors] head with per-class scores and no objectness, the ultralytics
    # v8/v11 export) or "yolov5" (the legacy [1, anchors, 5+nc] head with an
    # objectness column). Defaults to yolov8; a request that predates this field
    # decodes as yolov8.
    head: str = "yolov8"

    def to_dict(self) -> dict[str, Any]:
        return {
            "op": OP_LOAD_MODEL,
            "model_id": self.model_id,
            "path": self.path,
            "input_w": self.input_w,
            "input_h": self.input_h,
            "format": self.format,
            "class_labels": self.class_labels,
            "head": self.head,
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> LoadModelRequest:
        return cls(
            model_id=str(raw["model_id"]),
            path=str(raw["path"]),
            input_w=int(raw["input_w"]),
            input_h=int(raw["input_h"]),
            format=str(raw["format"]),
            class_labels=[str(c) for c in (raw.get("class_labels") or [])],
            head=str(raw.get("head") or "yolov8"),
        )


@dataclass
class InferRequest:
    """Ask the sidecar to run a resident model against one raw frame.

    ``frame`` is the raw pixel buffer in ``format`` at ``width`` x ``height``.
    """

    model_id: str
    frame: bytes
    width: int
    height: int
    format: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "op": OP_INFER,
            "model_id": self.model_id,
            "frame": self.frame,
            "width": self.width,
            "height": self.height,
            "format": self.format,
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> InferRequest:
        frame = raw.get("frame", b"")
        if isinstance(frame, (bytearray, memoryview)):
            frame = bytes(frame)
        elif not isinstance(frame, bytes):
            raise ProtocolError("infer.frame must be raw bytes")
        return cls(
            model_id=str(raw["model_id"]),
            frame=frame,
            width=int(raw["width"]),
            height=int(raw["height"]),
            format=str(raw["format"]),
        )


@dataclass
class EmbedRequest:
    """Ask the sidecar to run a resident re-id model against one cropped box and
    return its appearance embedding.

    ``crop`` is the raw pixel buffer of the box, already cropped + resized to the
    model's input (``crop_w`` x ``crop_h``) in ``format`` (the engine does the
    crop so the ONNX and RKNN paths consume identical bytes). The reply carries a
    flat ``embedding`` list of floats; the engine L2-normalizes it.
    """

    model_id: str
    crop: bytes
    crop_w: int
    crop_h: int
    format: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "op": OP_EMBED,
            "model_id": self.model_id,
            "crop": self.crop,
            "crop_w": self.crop_w,
            "crop_h": self.crop_h,
            "format": self.format,
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> EmbedRequest:
        crop = raw.get("crop", b"")
        if isinstance(crop, (bytearray, memoryview)):
            crop = bytes(crop)
        elif not isinstance(crop, bytes):
            raise ProtocolError("embed.crop must be raw bytes")
        return cls(
            model_id=str(raw["model_id"]),
            crop=crop,
            crop_w=int(raw["crop_w"]),
            crop_h=int(raw["crop_h"]),
            format=str(raw["format"]),
        )


def parse_request(raw: dict[str, Any]) -> LoadModelRequest | InferRequest | EmbedRequest:
    """Dispatch a decoded request mapping to its typed form by ``op``."""
    op = raw.get("op")
    if op == OP_LOAD_MODEL:
        return LoadModelRequest.from_dict(raw)
    if op == OP_INFER:
        return InferRequest.from_dict(raw)
    if op == OP_EMBED:
        return EmbedRequest.from_dict(raw)
    raise ProtocolError(f"unknown request op: {op!r}")


def detection_dict(
    *,
    x: float,
    y: float,
    width: float,
    height: float,
    class_label: str,
    confidence: float,
    track_id: int | None = None,
    assoc_confidence: float | None = None,
    lock_state: str | None = None,
) -> dict[str, Any]:
    """Build one detection mapping in the Rust ``Detection`` shape.

    The ``bbox`` is pixel-space, origin top-left, in the frame's own
    resolution, matching ``ados_protocol::framebus::BoundingBox``.

    ``assoc_confidence`` (0..1) and ``lock_state`` (``"locked"`` |
    ``"uncertain"`` | ``"lost"``) are optional and default-absent so a sidecar
    that does not score association round-trips with readers that predate them.
    """
    return {
        "bbox": {
            "x": float(x),
            "y": float(y),
            "width": float(width),
            "height": float(height),
        },
        "class_label": class_label,
        "confidence": float(confidence),
        "track_id": track_id,
        "assoc_confidence": (
            float(assoc_confidence) if assoc_confidence is not None else None
        ),
        "lock_state": lock_state,
    }


def ok_response(detections: list[dict[str, Any]] | None = None, **extra: Any) -> dict[str, Any]:
    """Build an ``ok`` response, defaulting ``detections`` to an empty list."""
    resp: dict[str, Any] = {"status": STATUS_OK, "detections": detections or []}
    resp.update(extra)
    return resp


def embedding_response(embedding: list[float], **extra: Any) -> dict[str, Any]:
    """Build an ``ok`` response carrying a flat re-id ``embedding`` (the engine
    L2-normalizes it)."""
    resp: dict[str, Any] = {"status": STATUS_OK, "embedding": [float(x) for x in embedding]}
    resp.update(extra)
    return resp


def error_response(message: str, **extra: Any) -> dict[str, Any]:
    """Build an ``error`` response carrying a human-readable ``error`` string.

    The engine treats any ``error`` status as a reason to fall back (for
    example, to a Rust-side ONNX Runtime path) rather than a fatal condition.
    """
    resp: dict[str, Any] = {"status": STATUS_ERROR, "error": message}
    resp.update(extra)
    return resp


# ---------------------------------------------------------------------------
# Frame codec
# ---------------------------------------------------------------------------


def encode_message(payload: dict[str, Any]) -> bytes:
    """Encode a request or response mapping as a length-prefixed msgpack frame.

    ``use_bin_type=True`` keeps Python ``bytes`` distinct from ``str`` on the
    wire, which is what the Rust ``rmp_serde`` decoder expects for the raw
    ``frame`` field.
    """
    body = msgpack.packb(payload, use_bin_type=True)
    if len(body) > MAX_FRAME_BYTES:
        raise ProtocolError(
            f"message body {len(body)} bytes exceeds cap {MAX_FRAME_BYTES}"
        )
    return len(body).to_bytes(HEADER_SIZE, "big") + body


def decode_message(body: bytes) -> dict[str, Any]:
    """Decode a msgpack frame body into a mapping."""
    raw = msgpack.unpackb(body, raw=False)
    if not isinstance(raw, dict):
        raise ProtocolError(f"frame body is not a mapping: {type(raw).__name__}")
    return raw


async def read_message(reader: asyncio.StreamReader) -> dict[str, Any] | None:
    """Read one length-prefixed msgpack frame from a stream.

    Returns ``None`` on a clean EOF before any header byte arrives (the peer
    closed between messages). Raises :class:`ProtocolError` on a truncated or
    oversized frame.
    """
    header = await _read_exact(reader, HEADER_SIZE)
    if header is None:
        return None
    length = int.from_bytes(header, "big")
    if length == 0:
        raise ProtocolError("frame length is zero")
    if length > MAX_FRAME_BYTES:
        raise ProtocolError(f"frame length {length} exceeds cap {MAX_FRAME_BYTES}")
    body = await _read_exact(reader, length)
    if body is None:
        raise ProtocolError("connection closed mid-frame")
    return decode_message(body)


async def write_message(writer: asyncio.StreamWriter, payload: dict[str, Any]) -> None:
    """Encode and flush one message to a stream."""
    writer.write(encode_message(payload))
    await writer.drain()


async def _read_exact(reader: asyncio.StreamReader, n: int) -> bytes | None:
    """Read exactly ``n`` bytes, or ``None`` on EOF before the first byte."""
    buf = b""
    while len(buf) < n:
        chunk = await reader.read(n - len(buf))
        if not chunk:
            return None if not buf else None
        buf += chunk
    return buf


# ---------------------------------------------------------------------------
# Server scaffold
# ---------------------------------------------------------------------------


class Backend(Protocol):
    """The inference surface a sidecar server drives.

    A backend turns typed requests into response mappings already in the
    protocol shape (see :func:`ok_response` / :func:`error_response`). Both the
    RKNN and TensorRT sidecars implement this so the socket plumbing is shared.
    """

    def load_model(self, req: LoadModelRequest) -> dict[str, Any]: ...

    def infer(self, req: InferRequest) -> dict[str, Any]: ...

    def embed(self, req: EmbedRequest) -> dict[str, Any]: ...


class SidecarServer:
    """An asyncio Unix-socket server that frames requests, dispatches them to a
    :class:`Backend`, and frames the response back.

    Each connection is handled serially: a request is read, the backend runs
    (in a thread so a blocking NPU/TensorRT call does not stall the event
    loop), and the response is written before the next request is read. A
    backend exception becomes an ``error`` response so one bad request never
    drops the connection.
    """

    def __init__(self, socket_path: str, backend: Backend, log: Any) -> None:
        self._socket_path = socket_path
        self._backend = backend
        self._log = log
        self._server: asyncio.AbstractServer | None = None

    async def serve_forever(self) -> None:
        path = Path(self._socket_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        # Remove a stale socket so bind does not fail with EADDRINUSE.
        with contextlib.suppress(FileNotFoundError):
            path.unlink()

        self._server = await asyncio.start_unix_server(
            self._handle_client, path=str(path)
        )
        with contextlib.suppress(OSError):
            os.chmod(str(path), 0o660)
        self._log.info("sidecar_listening", socket=str(path))

        try:
            async with self._server:
                await self._server.serve_forever()
        except asyncio.CancelledError:
            self._log.info("sidecar_stopping", socket=str(path))
        finally:
            with contextlib.suppress(FileNotFoundError):
                path.unlink()

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        try:
            while True:
                try:
                    raw = await read_message(reader)
                except ProtocolError as exc:
                    self._log.warning("sidecar_protocol_error", error=str(exc))
                    await self._safe_write(writer, error_response(str(exc)))
                    break
                if raw is None:
                    break
                response = await self._dispatch(raw)
                await write_message(writer, response)
        except (ConnectionResetError, BrokenPipeError):
            pass
        finally:
            with contextlib.suppress(Exception):
                writer.close()
                await writer.wait_closed()

    async def _dispatch(self, raw: dict[str, Any]) -> dict[str, Any]:
        try:
            req = parse_request(raw)
        except (ProtocolError, KeyError, ValueError) as exc:
            return error_response(f"bad request: {exc}")

        # The backend call may block (NPU / TensorRT), so run it off the loop.
        try:
            if isinstance(req, LoadModelRequest):
                return await asyncio.to_thread(self._backend.load_model, req)
            if isinstance(req, EmbedRequest):
                return await asyncio.to_thread(self._backend.embed, req)
            return await asyncio.to_thread(self._backend.infer, req)
        except Exception as exc:  # backend bug must not drop the connection
            self._log.error("sidecar_backend_error", error=str(exc))
            return error_response(f"backend error: {exc}")

    async def _safe_write(
        self, writer: asyncio.StreamWriter, payload: dict[str, Any]
    ) -> None:
        with contextlib.suppress(Exception):
            await write_message(writer, payload)
