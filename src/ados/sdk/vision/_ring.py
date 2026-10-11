"""Ring resolver: maps ``/dev/shm`` frame rings read-only, cached per shm_name."""

from __future__ import annotations

import contextlib
import mmap
import os
from pathlib import Path
from typing import Any

import msgpack

from ados.plugins.errors import PluginError
from ados.sdk.vision.detections import DetectionBatch
from ados.sdk.vision.frames import Frame, FrameDescriptor, RingLayout, read_slot

# ---------------------------------------------------------------------------
# Ring resolver (maps /dev/shm rings read-only, cached per shm_name).
# ---------------------------------------------------------------------------


class _MappedRing:
    """One memory-mapped frame ring: the read-only mmap, the layout recorded
    in its header, which file it is (``(st_dev, st_ino)`` of the descriptor
    that was mapped), and the highest sequence read out of it."""

    __slots__ = ("mm", "layout", "_fd", "file_id", "last_seq")

    def __init__(self, mm: mmap.mmap, layout: RingLayout, fd: Any) -> None:
        self.mm = mm
        self.layout = layout
        self._fd = fd
        st = os.fstat(fd.fileno())
        self.file_id = (st.st_dev, st.st_ino)
        self.last_seq = 0

    def read(self, descriptor: FrameDescriptor) -> bytes | None:
        """Read the descriptor's slot through the seqlock, recording its
        sequence when the read holds."""
        try:
            pixels = read_slot(
                memoryview(self.mm), self.layout, descriptor.slot, descriptor.seq
            )
        except ValueError:
            return None
        if pixels is not None:
            self.last_seq = max(self.last_seq, descriptor.seq)
        return pixels

    def is_named_by(self, path: Path) -> bool:
        """Whether ``path`` still names the file this mapping holds."""
        try:
            st = os.stat(path)
        except OSError:
            return False
        return (st.st_dev, st.st_ino) == self.file_id

    def close(self) -> None:
        with contextlib.suppress(Exception):
            self.mm.close()
        with contextlib.suppress(Exception):
            self._fd.close()


def _shm_dir() -> Path:
    """Directory holding POSIX shared-memory objects. ``/dev/shm`` on Linux."""
    return Path("/dev/shm")


def _map_ring(shm_name: str, *, shm_dir: Path | None = None) -> _MappedRing | None:
    """Map ``<shm_dir>/<shm_name>`` read-only and read the ring layout from its
    header. ``None`` if the file is missing, cannot be mapped, or has no valid
    header. A concurrent writer recycling slots is the expected case and is
    detected by the per-slot seqlock in :func:`read_slot`."""
    path = (shm_dir or _shm_dir()) / shm_name
    try:
        fd = open(path, "rb")
    except OSError:
        return None
    try:
        mm = mmap.mmap(fd.fileno(), 0, prot=mmap.PROT_READ)
    except (OSError, ValueError):
        with contextlib.suppress(Exception):
            fd.close()
        return None
    layout = RingLayout.read_header(memoryview(mm))
    if layout is None:
        with contextlib.suppress(Exception):
            mm.close()
            fd.close()
        return None
    return _MappedRing(mm, layout, fd)


class _RingResolver:
    """Resolves descriptors to frames, caching each ring's mapped region keyed
    by ``shm_name`` so a steady frame stream maps each ring once, not once per
    frame."""

    def __init__(self, *, shm_dir: Path | None = None) -> None:
        self._rings: dict[str, _MappedRing] = {}
        self._shm_dir = shm_dir

    def resolve(self, descriptor: FrameDescriptor) -> Frame | None:
        """Resolve a descriptor to a :class:`Frame`, mapping the ring on first
        sight of its ``shm_name``. Returns ``None`` on a torn/stale read or a
        ring that cannot be mapped (latest-wins; the frame is dropped).

        The engine replaces a ring by unlinking its file and creating a new one
        under the same name (every engine start does, sweeping the last run's
        rings), and the new ring's sequence starts over at 1. A mapping of the
        old file stays readable but never again holds the frame described (or,
        on a coincidental sequence, holds the previous run's pixels), which
        left a subscriber silent until its process restarted. So a sequence at
        or below one this mapping has served, or a slot it cannot read, is
        checked against the file the name names now, and a replaced ring is
        mapped afresh. A live stream pays nothing: its sequence only climbs and
        its reads hold."""
        path = (self._shm_dir or _shm_dir()) / descriptor.shm_name
        ring = self._rings.get(descriptor.shm_name)
        if ring is not None:
            regressed = descriptor.seq <= ring.last_seq
            if not regressed:
                pixels = ring.read(descriptor)
                if pixels is not None:
                    return Frame(descriptor=descriptor, pixels=pixels)
            if ring.is_named_by(path):
                # Still the live ring: the same descriptor resolved again (once
                # per matching subscription), or a slot the writer recycled.
                pixels = ring.read(descriptor) if regressed else None
                return None if pixels is None else Frame(descriptor, pixels)
            del self._rings[descriptor.shm_name]
            ring.close()
        ring = _map_ring(descriptor.shm_name, shm_dir=self._shm_dir)
        if ring is None:
            return None
        self._rings[descriptor.shm_name] = ring
        pixels = ring.read(descriptor)
        return None if pixels is None else Frame(descriptor, pixels)

    def close(self) -> None:
        for ring in self._rings.values():
            ring.close()
        self._rings.clear()


def _decode_descriptor(payload: Any) -> FrameDescriptor | None:
    """Decode a :class:`FrameDescriptor` from a ``vision.deliver`` event
    payload. The host carries the descriptor either as a ``descriptor`` binary
    blob or as the descriptor's own named fields; both decode through the same
    field mapping."""
    if not isinstance(payload, dict):
        return None
    blob = payload.get("descriptor")
    if isinstance(blob, (bytes, bytearray, memoryview)):
        try:
            return FrameDescriptor.from_msgpack(bytes(blob))
        except (ValueError, KeyError, msgpack.UnpackException):
            return None
    try:
        return FrameDescriptor.from_dict(payload)
    except (KeyError, ValueError, TypeError):
        return None


def _decode_infer_reply(args: Any) -> DetectionBatch:
    """Decode the ``vision.infer`` reply, ``{batch}``: the model's
    :class:`DetectionBatch` for the frame, as its msgpack blob (the shape
    ``ados_protocol::vision_rpc`` defines)."""
    raw = args.get("batch") if isinstance(args, dict) else None
    if not isinstance(raw, (bytes, bytearray, memoryview)):
        raise PluginError("infer reply decode failed: missing binary `batch`")
    try:
        return DetectionBatch.from_msgpack(bytes(raw))
    except (ValueError, KeyError, TypeError, msgpack.UnpackException) as exc:
        raise PluginError(f"infer reply decode failed: {exc}") from exc
