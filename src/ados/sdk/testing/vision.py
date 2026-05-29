"""Test harness for vision plugins.

Mirrors the Rust ``ados_sdk::testing`` ergonomics for the Python SDK: a plugin
author exercises a frame-consuming plugin without a real plugin host, vision
engine, shared memory, or socket. :class:`FakeVisionEngine` emits synthetic
frames (from in-memory pixel buffers or a directory of raw frame files) and
drives them through the same read path the production client uses, then
captures the :class:`~ados.sdk.vision.DetectionBatch` objects a plugin would
publish so a test can assert against them.

The fake builds a real frame ring through the shared frame-transport contract
and resolves each synthetic frame the same way the production client does
(per-slot seqlock, latest-wins, torn/stale reads dropped), so a plugin's
frame-handling path is exercised end to end; only the host and the OS
shared-memory object are faked.

Two delivery modes:

* In-process: register a callback with :meth:`FakeVisionEngine.on_frame`, push
  frames, then :meth:`FakeVisionEngine.deliver_all`. The engine writes each
  frame into its in-memory ring and resolves it through the seqlock, exactly
  as the production resolver does.
* End-to-end through the real client: :meth:`FakeVisionEngine.attach` wires a
  real :class:`~ados.sdk.vision.VisionClient` (backed by a
  :class:`~ados.sdk.testing.stubs.FakeIpcClient`) so the ring lives in a temp
  directory the client maps read-only and the descriptor rides a
  ``vision.deliver`` event into the client's resolver.
"""

from __future__ import annotations

import tempfile
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

from ados.sdk.testing.stubs import FakeIpcClient
from ados.sdk.vision import (
    DELIVER_FRAME,
    VISION_FRAME_TOPIC,
    DetectionBatch,
    Frame,
    FrameCallback,
    FrameDescriptor,
    FrameFormat,
    RingLayout,
    VisionClient,
    read_slot,
    write_slot,
)

# Default slot count for the fake ring. Large enough that the harness never
# recycles a slot under a pending read in a single-threaded test.
DEFAULT_SLOT_COUNT = 8

# The capture timestamp base, matching the Rust harness so cross-language
# round-trip assertions line up.
_TS_BASE_MS = 1_700_000_000_000


class FakeVisionEngine:
    """In-process stand-in for the vision engine + plugin host bridge.

    Owns a synthetic frame ring and a queue of pending frames. A test
    registers a callback with :meth:`on_frame`, enqueues synthetic frames, then
    calls :meth:`deliver_all` (or :meth:`deliver_one`) to drive them through the
    resolver into the callback. Detections the plugin publishes are captured via
    :meth:`captured_detections`.
    """

    def __init__(
        self,
        camera_id: str,
        width: int,
        height: int,
        fmt: FrameFormat,
        *,
        slot_count: int = DEFAULT_SLOT_COUNT,
        shm_dir: Path | None = None,
    ) -> None:
        self.camera_id = camera_id
        self.shm_name = f"ados-vision-{camera_id}"
        self.format = fmt
        self.layout = RingLayout.for_frame(slot_count, width, height, fmt)
        # When a shm_dir is given the ring is file-backed there (for the
        # end-to-end client path); otherwise it is a pure in-memory bytearray.
        self._shm_dir = shm_dir
        self._region = bytearray(self.layout.total_len())
        self.layout.write_header(self._region)
        if shm_dir is not None:
            self._ring_path: Path | None = Path(shm_dir) / self.shm_name
            self._flush_region()
        else:
            self._ring_path = None
        # Monotonic frame sequence, also the ring slot via seq % slot_count.
        self._next_seq = 0
        self._pending: list[bytes] = []
        self._callback: FrameCallback | None = None
        self._captured: list[DetectionBatch] = []
        # End-to-end wiring, set by attach().
        self._client: VisionClient | None = None
        self._ipc: FakeIpcClient | None = None

    # ------------------------------------------------------------------
    # Ring sizing
    # ------------------------------------------------------------------

    def frame_bytes(self) -> int:
        """The frame size of one full frame in this ring's format."""
        return self.layout.slot_bytes

    # ------------------------------------------------------------------
    # Callback registration (in-process mode)
    # ------------------------------------------------------------------

    def on_frame(self, callback: FrameCallback) -> None:
        """Register the per-frame callback the plugin under test would pass to
        ``ctx.vision.subscribe_frames``. Replaces any prior callback."""
        self._callback = callback

    # ------------------------------------------------------------------
    # Frame enqueue
    # ------------------------------------------------------------------

    def push_frame(self, pixels: bytes) -> None:
        """Enqueue a raw pixel frame. The bytes must be at most one full frame
        (:meth:`frame_bytes`); a shorter slice is delivered as a partial frame,
        which is what a real engine does for a truncated capture."""
        self._pending.append(bytes(pixels))

    def push_solid(self, value: int) -> None:
        """Enqueue a frame filled with one byte value (a flat colour), sized to
        a full frame. Handy for asserting the callback sees the right bytes."""
        self._pending.append(bytes([value & 0xFF]) * self.frame_bytes())

    def push_dir(self, directory: str | Path) -> int:
        """Enqueue every ``*.bin`` / ``*.raw`` file in ``directory`` (sorted by
        name) as a frame, reading each file's bytes verbatim. Returns the count
        enqueued."""
        paths = sorted(
            p
            for p in Path(directory).iterdir()
            if p.suffix in (".bin", ".raw")
        )
        for p in paths:
            self._pending.append(p.read_bytes())
        return len(paths)

    # ------------------------------------------------------------------
    # Delivery
    # ------------------------------------------------------------------

    def _make_descriptor(self, seq: int, slot: int, byte_len: int) -> FrameDescriptor:
        return FrameDescriptor(
            camera_id=self.camera_id,
            frame_id=seq,
            ts_ms=_TS_BASE_MS + seq,
            # width/height are descriptive; the resolver keys on byte_len.
            width=0,
            height=0,
            format=self.format,
            shm_name=self.shm_name,
            slot=slot,
            seq=seq,
            byte_len=byte_len,
        )

    def _write_next(self) -> FrameDescriptor:
        """Pop the next pending frame, write it into the ring, and return its
        descriptor. Writes through to the file-backed ring when end-to-end."""
        pixels = self._pending.pop(0)
        seq = self._next_seq + 1
        self._next_seq = seq
        slot = seq % self.layout.slot_count
        write_slot(self._region, self.layout, slot, seq, pixels)
        if self._ring_path is not None:
            self._flush_region()
        return self._make_descriptor(seq, slot, len(pixels))

    async def deliver_one(self) -> bool:
        """Deliver the next pending frame: write it into the ring, build its
        descriptor, resolve it through the seqlock, and invoke the registered
        callback (or, when attached, push it through the real client). Returns
        ``True`` if a frame was consumed, ``False`` if the queue was empty."""
        if not self._pending:
            return False
        descriptor = self._write_next()

        if self._ipc is not None:
            # End-to-end: hand the descriptor to the client over the same
            # vision.deliver event the host would publish; the client's
            # resolver maps the file-backed ring and fires the plugin callback.
            await self._ipc.deliver(
                VISION_FRAME_TOPIC,
                {"method": DELIVER_FRAME, "descriptor": descriptor.to_msgpack()},
            )
            return True

        # In-process: resolve against the in-memory ring exactly as the
        # production resolver does and fire the callback directly.
        pixels = read_slot(
            self._region, self.layout, descriptor.slot, descriptor.seq
        )
        if pixels is not None and self._callback is not None:
            result = self._callback(Frame(descriptor=descriptor, pixels=pixels))
            if hasattr(result, "__await__"):
                await result  # type: ignore[union-attr]
        return True

    async def deliver_all(self) -> int:
        """Deliver every pending frame in order. Returns the number delivered."""
        n = 0
        while await self.deliver_one():
            n += 1
        return n

    # ------------------------------------------------------------------
    # End-to-end client wiring
    # ------------------------------------------------------------------

    async def attach(
        self,
        callback: FrameCallback,
        *,
        camera_id: str | None = None,
        granted_capabilities: set[str] | None = None,
    ) -> VisionClient:
        """Wire a real :class:`VisionClient` over a :class:`FakeIpcClient` and
        subscribe ``callback`` to frames. Subsequent :meth:`deliver_one` /
        :meth:`deliver_all` calls drive frames through the client's resolver and
        the production ``vision.deliver`` event path.

        Requires the engine to have been built with a ``shm_dir`` so the ring
        is file-backed where the client can map it. Returns the client.
        """
        if self._ring_path is None:
            raise ValueError(
                "attach() requires a file-backed ring; build the engine with "
                "shm_dir=<dir> (use FakeVisionEngine.with_shm_dir)"
            )
        caps = set(granted_capabilities or set())
        caps |= {
            "event.subscribe",
            "vision.frame.read",
            "vision.model.register",
            "vision.detection.publish",
        }
        self._ipc = FakeIpcClient(
            plugin_id="com.example.vision-test", granted_capabilities=caps
        )
        self._client = VisionClient(self._ipc, shm_dir=Path(self._shm_dir))
        await self._client.subscribe_frames(callback, camera_id=camera_id)
        return self._client

    @classmethod
    def with_shm_dir(
        cls,
        camera_id: str,
        width: int,
        height: int,
        fmt: FrameFormat,
        *,
        slot_count: int = DEFAULT_SLOT_COUNT,
    ) -> FakeVisionEngine:
        """Build an engine whose ring is file-backed in a fresh temp directory,
        ready for :meth:`attach`. The directory lives for the engine's life."""
        tmp = tempfile.mkdtemp(prefix="ados-fake-vision-")
        return cls(
            camera_id,
            width,
            height,
            fmt,
            slot_count=slot_count,
            shm_dir=Path(tmp),
        )

    # ------------------------------------------------------------------
    # Detection capture
    # ------------------------------------------------------------------

    def capture(self, batch: DetectionBatch) -> None:
        """Record a detection batch the plugin under test published, as if the
        engine received it."""
        self._captured.append(batch)

    def captured_detections(self) -> list[DetectionBatch]:
        """The captured detections in publish order. Returns a copy."""
        return list(self._captured)

    def clear_captured(self) -> None:
        self._captured.clear()

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    def _flush_region(self) -> None:
        assert self._ring_path is not None
        self._ring_path.write_bytes(bytes(self._region))

    def close(self) -> None:
        """Release the client's cached mmaps. Idempotent."""
        if self._client is not None:
            self._client.close()


# A handler shape a test may use for clarity.
DetectionHandler = Callable[[DetectionBatch], Awaitable[None] | None]

__all__ = ["FakeVisionEngine", "DetectionHandler"]
