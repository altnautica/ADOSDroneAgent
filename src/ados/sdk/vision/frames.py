"""Frame format, descriptor, and the shared-memory ring layout + seqlock."""

from __future__ import annotations

import enum
import struct
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

import msgpack

if TYPE_CHECKING:
    from ados.sdk.vision.detections import DetectionBatch


# ---------------------------------------------------------------------------
# Frame format + descriptor.
# ---------------------------------------------------------------------------


class FrameFormat(str, enum.Enum):
    """Normalized pixel format of a frame in the ring. The engine downscales
    and converts the camera's native format to one of these before publishing.

    A ``str`` enum so the member value is exactly the lowercase string the
    msgpack wire carries, matching the Rust ``lowercase`` rename.
    """

    RGB24 = "rgb24"
    """Packed 24-bit RGB, 3 bytes per pixel."""
    NV12 = "nv12"
    """Semi-planar YUV 4:2:0 (Y plane then interleaved UV), 1.5 bytes/pixel."""
    YUV420P = "yuv420p"
    """Planar YUV 4:2:0 (Y, then U, then V), 1.5 bytes per pixel."""

    def frame_bytes(self, width: int, height: int) -> int:
        """Exact byte length of one ``width`` x ``height`` frame in this format.

        The 4:2:0 formats require even dimensions; callers normalize to even
        width/height before sizing a ring.
        """
        px = int(width) * int(height)
        if self is FrameFormat.RGB24:
            return px * 3
        # Y plane (px) + chroma (px // 2) = px * 3 // 2.
        return px + px // 2


@dataclass(frozen=True)
class FrameDescriptor:
    """The small message published on ``vision.frame``. It names the ring slot
    a consumer should read and carries the ``seq`` that the per-slot seqlock
    must still hold for the read to be valid.

    The msgpack field names match the shared frame-transport contract exactly
    so a descriptor encoded here decodes in Rust and vice versa.
    """

    camera_id: str
    """Source camera id. Lets a consumer filter by camera."""
    frame_id: int
    """Monotonic frame counter for this camera, starting at 1."""
    ts_ms: int
    """Capture time in milliseconds (the clock the engine timestamps with)."""
    width: int
    height: int
    format: FrameFormat
    shm_name: str
    """``/dev/shm`` name of the ring this frame lives in (one ring per camera)."""
    slot: int
    """Slot index within the ring holding this frame's pixels."""
    seq: int
    """Ring sequence stamped on the slot; re-checked against the slot's
    seqlock after copying. A mismatch means a torn read and the frame is
    dropped."""
    byte_len: int
    """Length of the valid pixel bytes in the slot (``format.frame_bytes``)."""

    def to_dict(self) -> dict[str, Any]:
        """The msgpack-named mapping. ``format`` is the lowercase string."""
        return {
            "camera_id": self.camera_id,
            "frame_id": int(self.frame_id),
            "ts_ms": int(self.ts_ms),
            "width": int(self.width),
            "height": int(self.height),
            "format": FrameFormat(self.format).value,
            "shm_name": self.shm_name,
            "slot": int(self.slot),
            "seq": int(self.seq),
            "byte_len": int(self.byte_len),
        }

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> FrameDescriptor:
        return cls(
            camera_id=str(raw["camera_id"]),
            frame_id=int(raw["frame_id"]),
            ts_ms=int(raw["ts_ms"]),
            width=int(raw["width"]),
            height=int(raw["height"]),
            format=FrameFormat(str(raw["format"])),
            shm_name=str(raw["shm_name"]),
            slot=int(raw["slot"]),
            seq=int(raw["seq"]),
            byte_len=int(raw["byte_len"]),
        )

    def to_msgpack(self) -> bytes:
        return msgpack.packb(self.to_dict(), use_bin_type=True)

    @classmethod
    def from_msgpack(cls, blob: bytes) -> FrameDescriptor:
        raw = msgpack.unpackb(blob, raw=False)
        if not isinstance(raw, dict):
            raise ValueError("frame descriptor is not a msgpack mapping")
        return cls.from_dict(raw)


@dataclass(frozen=True)
class Frame:
    """A resolved camera frame: the descriptor the engine published plus the
    pixel bytes read out of the shared-memory ring it named.

    ``len(pixels)`` equals ``descriptor.byte_len`` and is the valid pixel data
    for ``descriptor.width`` x ``descriptor.height`` in ``descriptor.format``.
    """

    descriptor: FrameDescriptor
    pixels: bytes


# A callback invoked once per resolved frame. A frame the ring could not
# resolve (torn or stale read, or a ring that vanished) is dropped silently and
# the callback does not fire for it.
FrameCallback = Callable[[Frame], Awaitable[None] | None]


# A callback invoked once per delivered detection batch. The batch is decoded
# from the host's ``vision.deliver_detection`` event; a batch that fails to
# decode is dropped silently and the callback does not fire for it.
DetectionCallback = Callable[["DetectionBatch"], Awaitable[None] | None]


# ---------------------------------------------------------------------------
# Ring layout + seqlock read (mirrors the shared frame-transport contract).
# ---------------------------------------------------------------------------


# "ADV1" — ADOS vision ring, version 1.
_RING_MAGIC = 0x41445631
_RING_VERSION = 1
_HEADER_LEN = 16
# seq_begin (u64) + byte_len (u32) + pad (u32).
_SLOT_HEADER_LEN = 16
# seq_end (u64).
_SLOT_TRAILER_LEN = 8

_U64 = struct.Struct("<Q")
_U32 = struct.Struct("<I")
_HEADER = struct.Struct("<IHHII")


@dataclass(frozen=True)
class RingLayout:
    """Memory layout of a single-writer, many-reader frame ring.

    Layout (all integers little-endian)::

        [ ring header ]  16 bytes: magic, version, slot_count, slot_bytes, pad
        [ slot 0      ]  seq_begin:u64 | byte_len:u32 | pad:u32 | data | seq_end:u64
        [ slot 1      ]
          ...

    The writer stores ``seq_begin``, then the data and length, then ``seq_end``;
    a reader loads ``seq_end``, copies the data, then re-checks ``seq_begin`` and
    ``seq_end`` against the descriptor's ``seq``. If either differs the read was
    torn by a slot recycle and is discarded.
    """

    slot_count: int
    slot_bytes: int

    @classmethod
    def for_frame(
        cls, slot_count: int, width: int, height: int, fmt: FrameFormat
    ) -> RingLayout:
        return cls(
            slot_count=int(slot_count),
            slot_bytes=fmt.frame_bytes(width, height),
        )

    def slot_stride(self) -> int:
        return _SLOT_HEADER_LEN + self.slot_bytes + _SLOT_TRAILER_LEN

    def total_len(self) -> int:
        return _HEADER_LEN + self.slot_count * self.slot_stride()

    def slot_offset(self, slot: int) -> int:
        return _HEADER_LEN + slot * self.slot_stride()

    def write_header(self, region: bytearray | memoryview) -> None:
        """Write the ring header at the front of a freshly created region."""
        if len(region) < self.total_len():
            raise ValueError(
                f"shared region is {len(region)} bytes, layout needs "
                f"{self.total_len()}"
            )
        region[0:_HEADER_LEN] = _HEADER.pack(
            _RING_MAGIC,
            _RING_VERSION,
            self.slot_count & 0xFFFF,
            self.slot_bytes & 0xFFFFFFFF,
            0,
        )

    @classmethod
    def read_header(cls, region: bytes | memoryview) -> RingLayout | None:
        """Read the layout a writer recorded in a region's header. ``None`` if
        the region is too small or the magic/version do not match."""
        if len(region) < _HEADER_LEN:
            return None
        magic, version, slot_count, slot_bytes, _pad = _HEADER.unpack(
            bytes(region[0:_HEADER_LEN])
        )
        if magic != _RING_MAGIC or version != _RING_VERSION:
            return None
        return cls(slot_count=slot_count, slot_bytes=slot_bytes)


def write_slot(
    region: bytearray | memoryview,
    layout: RingLayout,
    slot: int,
    seq: int,
    data: bytes,
) -> None:
    """Write one frame into ``slot`` of the ring, stamping it with ``seq``.

    The single writer chooses ``slot = seq % slot_count`` and calls this once
    per captured frame, then publishes the matching :class:`FrameDescriptor`.
    """
    if not 0 <= slot < layout.slot_count:
        raise ValueError(
            f"slot {slot} out of range (ring has {layout.slot_count} slots)"
        )
    cap = layout.slot_bytes
    if len(data) > cap:
        raise ValueError(
            f"payload of {len(data)} bytes exceeds slot capacity {cap}"
        )
    if len(region) < layout.total_len():
        raise ValueError(
            f"shared region is {len(region)} bytes, layout needs "
            f"{layout.total_len()}"
        )

    base = layout.slot_offset(slot)
    data_off = base + _SLOT_HEADER_LEN
    trailer_off = data_off + cap

    # seq_begin first (marks the slot as being written for this seq).
    region[base : base + 8] = _U64.pack(seq)
    region[base + 8 : base + 12] = _U32.pack(len(data))
    region[base + 12 : base + 16] = _U32.pack(0)
    region[data_off : data_off + len(data)] = data
    # seq_end last (commits the write).
    region[trailer_off : trailer_off + 8] = _U64.pack(seq)


def read_slot(
    region: bytes | memoryview,
    layout: RingLayout,
    slot: int,
    expected_seq: int,
) -> bytes | None:
    """Read the frame a :class:`FrameDescriptor` points at, validating the
    seqlock.

    Returns ``None`` if the slot no longer holds ``expected_seq`` (the writer
    recycled it) so the caller drops that frame and waits for the next
    descriptor.
    """
    if not 0 <= slot < layout.slot_count:
        raise ValueError(
            f"slot {slot} out of range (ring has {layout.slot_count} slots)"
        )
    if len(region) < layout.total_len():
        raise ValueError(
            f"shared region is {len(region)} bytes, layout needs "
            f"{layout.total_len()}"
        )

    cap = layout.slot_bytes
    base = layout.slot_offset(slot)
    data_off = base + _SLOT_HEADER_LEN
    trailer_off = data_off + cap

    # Load the trailer (committed marker) first.
    (seq_end,) = _U64.unpack(bytes(region[trailer_off : trailer_off + 8]))
    if seq_end != expected_seq:
        return None
    (byte_len,) = _U32.unpack(bytes(region[base + 8 : base + 12]))
    if byte_len > cap:
        return None
    data = bytes(region[data_off : data_off + byte_len])
    # Re-check both guards: a writer that recycled this slot mid-copy moves the
    # seq forward, so a stale begin or a changed end means the copy was torn.
    (seq_begin,) = _U64.unpack(bytes(region[base : base + 8]))
    (seq_end2,) = _U64.unpack(bytes(region[trailer_off : trailer_off + 8]))
    if seq_begin != expected_seq or seq_end2 != expected_seq:
        return None
    return data
