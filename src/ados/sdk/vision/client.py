"""VisionClient: the ``ctx.vision`` facade."""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Any

import msgpack

from ados.sdk.vision._constants import (
    DESIGNATE_TRACK,
    INFER,
    PUBLISH_DETECTION,
    REGISTER_MODEL,
    SUBSCRIBE_DETECTIONS,
    SUBSCRIBE_FRAMES,
    VIO_COMPONENT_ID,
    VISION_FRAME_TOPIC,
)
from ados.sdk.vision._ring import (
    _decode_descriptor,
    _decode_infer_reply,
    _RingResolver,
)
from ados.sdk.vision.detections import (
    BoundingBox,
    Detection,
    DetectionBatch,
    ModelMetadata,
)
from ados.sdk.vision.frames import (
    DetectionCallback,
    Frame,
    FrameCallback,
)
from ados.sdk.vision.pose import Odometry, Pose

if TYPE_CHECKING:
    from ados.plugins.ipc_client import PluginIpcClient


# ---------------------------------------------------------------------------
# VisionClient — ctx.vision facade.
# ---------------------------------------------------------------------------


class VisionClient:
    """``ctx.vision`` — the vision engine facade.

    Mirrors the host RPC surface for frame subscription, model registration,
    inference, and detection publishing, plus the visual-odometry pose helper.
    The ring resolver caches each camera's mapped ``/dev/shm`` region keyed by
    ``shm_name``.

    The client gates nothing itself; the host enforces ``vision.frame.read``,
    ``vision.model.register``, and ``vision.detection.publish``.
    """

    def __init__(
        self, ipc: PluginIpcClient, *, shm_dir: Path | None = None
    ) -> None:
        self._ipc = ipc
        self._resolver = _RingResolver(shm_dir=shm_dir)

    async def _request(
        self, method: str, capability: str, args: dict[str, Any]
    ) -> dict:
        """Send one vision RPC through whatever sender the IPC exposes.

        The IPC client owns the wire (request id, token, capability tag). This
        prefers a typed ``vision_*`` method when the client provides one and
        otherwise calls the generic request sender, shaping the args the host
        gates the method on. The host enforces the capability; this only tags
        it on the envelope.
        """
        send = getattr(self._ipc, "_send_request", None)
        if send is not None:
            env = await send(method, capability=capability, args=args)
            return getattr(env, "args", {})
        # Duck-typed fallback for stubs that expose a request shim directly.
        return await self._ipc.vision_request(  # type: ignore[attr-defined]
            method, capability=capability, args=args
        )

    async def subscribe_frames(
        self,
        callback: FrameCallback,
        *,
        camera_id: str | None = None,
    ) -> None:
        """Subscribe to frames, optionally filtered to one ``camera_id``.

        Sends the subscribe-frames RPC (gated on ``vision.frame.read``) then
        registers an event subscriber on the ``vision.frame`` topic. A
        ``camera_id`` of ``None`` receives every camera's frames; a filter is
        applied both in the RPC argument (so the host can narrow the stream)
        and in the resolver (so a broader host stream is still filtered
        locally). The host delivers matching descriptors as ``vision.deliver``
        events; this client resolves each to pixels and invokes ``callback``
        with the :class:`Frame`.
        """
        want = camera_id

        async def _on_event(payload: dict[str, Any]) -> None:
            descriptor = _decode_descriptor(payload)
            if descriptor is None:
                return
            if want is not None and descriptor.camera_id != want:
                return
            frame = self._resolver.resolve(descriptor)
            if frame is None:
                return
            result = callback(frame)
            if hasattr(result, "__await__"):
                await result  # type: ignore[union-attr]

        # Tell the engine to start (or widen) the stream toward this plugin.
        sub_args: dict[str, Any] = {}
        if camera_id is not None:
            sub_args["camera_id"] = camera_id
        await self._request(SUBSCRIBE_FRAMES, "vision.frame.read", sub_args)
        # Frame descriptors arrive as events on the reserved frame topic.
        await self._ipc.event_subscribe(VISION_FRAME_TOPIC, _on_event)

    async def subscribe_detections(
        self,
        callback: DetectionCallback,
        *,
        camera_id: str | None = None,
    ) -> None:
        """Subscribe to published detection batches, optionally filtered to one
        ``camera_id``. Gated on ``vision.detection.subscribe``.

        Sends the subscribe-detections RPC then registers a deliver handler. A
        ``camera_id`` of ``None`` receives every camera's batches; a filter is
        applied both in the RPC argument (so the host can narrow the stream) and
        locally (so a broader host stream is still filtered). The host delivers
        each batch as a ``vision.deliver_detection`` event whose ``batch`` field
        is the encoded :class:`DetectionBatch`; this decodes it and invokes
        ``callback`` with the batch.

        The deliver event carries no ``topic`` field, so it is routed explicitly
        by the IPC reader by method name (like ``mavlink.deliver``) rather than
        through the topic-based event surface.
        """
        want = camera_id

        async def _on_batch(payload: dict[str, Any]) -> None:
            raw = payload.get("batch")
            if not isinstance(raw, (bytes, bytearray, memoryview)):
                return
            try:
                batch = DetectionBatch.from_msgpack(bytes(raw))
            except (ValueError, KeyError, msgpack.UnpackException):
                return
            if want is not None and batch.camera_id != want:
                return
            result = callback(batch)
            if hasattr(result, "__await__"):
                await result  # type: ignore[union-attr]

        sub_args: dict[str, Any] = {}
        if camera_id is not None:
            sub_args["camera_id"] = camera_id
        await self._request(
            SUBSCRIBE_DETECTIONS, "vision.detection.subscribe", sub_args
        )
        await self._ipc.vision_subscribe_detections(_on_batch)

    async def designate_track(
        self,
        camera_id: str,
        bbox: BoundingBox,
        *,
        class_label: str = "",
        confidence: float = 1.0,
    ) -> dict:
        """Designate the engine's single follow target on ``camera_id`` (the
        operator's click-to-follow pick), overriding the auto-lock. Gated on
        ``vision.track.designate``. Returns the engine's response args
        ``{designated: bool, track_id: int|None, camera_id: str}``."""
        return await self._request(
            DESIGNATE_TRACK,
            "vision.track.designate",
            {
                "camera_id": camera_id,
                "bbox": bbox.to_dict(),
                "class_label": class_label,
                "confidence": float(confidence),
            },
        )

    async def register_model(self, model: ModelMetadata) -> dict:
        """Register an inference model with the engine. Carries the metadata as
        a msgpack blob the engine decodes. Gated on ``vision.model.register``.
        """
        return await self._request(
            REGISTER_MODEL,
            "vision.model.register",
            {"model": model.to_msgpack()},
        )

    async def infer(self, model_id: str, frame: Frame) -> list[Detection]:
        """Run a registered model against one frame on the shared backend and
        return its detections. Gated on ``vision.model.register``. The frame is
        passed by descriptor (the engine reads its own ring), so no pixels
        cross the RPC envelope. A frame whose slot the ring has since recycled
        is an error; infer on a fresher frame."""
        resp = await self._request(
            INFER,
            "vision.model.register",
            {
                "model_id": model_id,
                "descriptor": frame.descriptor.to_msgpack(),
            },
        )
        return _decode_infer_reply(resp).detections

    async def publish_detection(self, batch: DetectionBatch) -> dict:
        """Publish a detection batch on ``vision.detection``. Carries the batch
        as a msgpack blob. Gated on ``vision.detection.publish``."""
        return await self._request(
            PUBLISH_DETECTION,
            "vision.detection.publish",
            {"batch": batch.to_msgpack()},
        )

    async def publish_one(
        self, model_id: str, frame: Frame, detection: Detection
    ) -> dict:
        """Publish a single detection against one frame, building the
        :class:`DetectionBatch` from the frame's source camera and id. A
        convenience over :meth:`publish_detection` for the common
        one-box-per-frame case."""
        batch = DetectionBatch(
            model_id=model_id,
            camera_id=frame.descriptor.camera_id,
            frame_id=frame.descriptor.frame_id,
            ts_ms=frame.descriptor.ts_ns // 1_000_000,
            detections=[detection],
            # Carry the source frame's size so a consumer scales the pixel-space
            # box to the rendered frame instead of guessing.
            frame_width=frame.descriptor.width,
            frame_height=frame.descriptor.height,
        )
        return await self.publish_detection(batch)

    async def register_vio_component(self) -> dict:
        """Register this plugin as the visual-odometry MAVLink component so the
        FC attributes injected pose to a vision source. Call once before
        :meth:`inject_pose` / :meth:`inject_odometry`."""
        return await self._ipc.mavlink_register_component(
            VIO_COMPONENT_ID, "vio"
        )

    async def inject_pose(self, pose: Pose) -> dict:
        """Build a VISION_POSITION_ESTIMATE from ``pose`` and send it to the FC
        over the host's MAVLink path under the visual-odometry component id."""
        frame = pose.to_vision_position_estimate_frame()
        return await self._ipc.mavlink_send(
            frame, component_id=VIO_COMPONENT_ID
        )

    async def inject_odometry(self, odometry: Odometry) -> dict:
        """Build an ODOMETRY message from ``odometry`` (pose plus body-frame
        twist) and send it to the FC under the visual-odometry component id."""
        frame = odometry.to_odometry_frame()
        return await self._ipc.mavlink_send(
            frame, component_id=VIO_COMPONENT_ID
        )

    def close(self) -> None:
        """Release the cached ring mmaps. Idempotent."""
        self._resolver.close()
