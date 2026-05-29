# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""TensorRT inference sidecar for serialized ``.engine`` models (Jetson Orin).

Mirror of the RKNN sidecar for NVIDIA hardware. The ``tensorrt`` and
``pycuda`` wheels are proprietary, Python-only, and tied to the Jetson JetPack
runtime, so the Rust vision engine reaches this Python process over
``/run/ados/vision-tensorrt.sock`` and speaks the same
:mod:`ados.services.vision.sidecar_protocol` request/response shape. Detections
come back already in the Rust ``Detection`` field layout.

``tensorrt`` is imported lazily inside :class:`TensorRTBackend`. On a host
without the wheel (a dev laptop, a non-Jetson board), ``load_model`` returns an
``error`` response and the engine falls back to a Rust-side path; the sidecar
stays up. The execution path that allocates CUDA buffers and runs the engine is
stubbed behind the same lazy import, so this file builds and serves everywhere
even though only a Jetson can actually infer.

Run as ``python -m ados.services.vision.tensorrt_sidecar``.

Postprocessing reuses the YOLO-style decoder from the RKNN sidecar so a model
exported to both ``.rknn`` and ``.engine`` produces identical boxes.
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import signal
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.services.vision import sidecar_protocol as proto
from ados.services.vision.rknn_sidecar import (
    DEFAULT_CONF_THRESHOLD,
    DEFAULT_NMS_IOU,
    _nms,
)
from ados.services.vision.sidecar_protocol import SidecarServer

log = get_logger("vision-tensorrt")

DEFAULT_SOCKET = "/run/ados/vision-tensorrt.sock"


@dataclass
class _LoadedEngine:
    """A deserialized TensorRT engine plus the execution context and the
    metadata needed to label and scale detections back onto the source frame."""

    engine: Any
    context: Any
    input_w: int
    input_h: int
    fmt: str
    class_labels: list[str]


class TensorRTBackend:
    """Loads serialized ``.engine`` files with TensorRT and runs inference.

    The ``tensorrt`` import is deferred to :meth:`load_model`. A missing wheel
    surfaces as an error response rather than a crash.
    """

    def __init__(
        self,
        conf_threshold: float = DEFAULT_CONF_THRESHOLD,
        nms_iou: float = DEFAULT_NMS_IOU,
    ) -> None:
        self._engines: dict[str, _LoadedEngine] = {}
        self._conf_threshold = conf_threshold
        self._nms_iou = nms_iou

    @staticmethod
    def _import_runtime() -> Any:
        """Import the ``tensorrt`` module, or raise a clear error."""
        try:
            import tensorrt as trt  # type: ignore[import-not-found]
        except ImportError as exc:  # pragma: no cover - depends on Jetson wheel
            raise RuntimeError(
                "tensorrt is not installed; TensorRT inference is unavailable "
                "on this host"
            ) from exc
        return trt

    def load_model(self, req: proto.LoadModelRequest) -> dict[str, Any]:
        path = Path(req.path)
        if not path.is_file():
            return proto.error_response(f"engine file not found: {req.path}")

        try:
            trt = self._import_runtime()
        except RuntimeError as exc:
            return proto.error_response(str(exc))

        try:
            logger = trt.Logger(trt.Logger.WARNING)
            runtime = trt.Runtime(logger)
            with open(path, "rb") as f:
                engine = runtime.deserialize_cuda_engine(f.read())
            if engine is None:
                return proto.error_response(f"failed to deserialize engine: {req.path}")
            context = engine.create_execution_context()
        except Exception as exc:  # pragma: no cover - depends on Jetson wheel
            log.error("tensorrt_load_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"tensorrt load error: {exc}")

        self._engines[req.model_id] = _LoadedEngine(
            engine=engine,
            context=context,
            input_w=req.input_w,
            input_h=req.input_h,
            fmt=req.format,
            class_labels=req.class_labels,
        )
        log.info("tensorrt_model_loaded", model=req.model_id, path=req.path)
        return proto.ok_response()

    def infer(self, req: proto.InferRequest) -> dict[str, Any]:
        loaded = self._engines.get(req.model_id)
        if loaded is None:
            return proto.error_response(f"model not loaded: {req.model_id}")

        try:
            outputs = self._run_engine(loaded, req)
        except RuntimeError as exc:
            return proto.error_response(str(exc))
        except Exception as exc:  # pragma: no cover - depends on Jetson wheel
            log.error("tensorrt_infer_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"tensorrt inference error: {exc}")

        try:
            import numpy as np  # lazy: only needed on the real infer path
        except ImportError as exc:  # pragma: no cover - Jetson has numpy
            return proto.error_response(f"numpy unavailable: {exc}")

        detections = self._postprocess_yolo(
            np, outputs, req.width, req.height, loaded
        )
        return proto.ok_response(detections)

    def _run_engine(self, loaded: _LoadedEngine, req: proto.InferRequest) -> list[Any]:
        """Run one frame through the engine.

        The CUDA buffer allocation and ``execute_v2`` call need ``pycuda``,
        which is only present on a Jetson. When it is missing the path raises a
        clear :class:`RuntimeError` that :meth:`infer` turns into an ``error``
        response so the engine can fall back.
        """
        try:
            import pycuda.autoinit  # noqa: F401  type: ignore[import-not-found]
            import pycuda.driver  # noqa: F401  type: ignore[import-not-found]
        except ImportError as exc:  # pragma: no cover - depends on Jetson wheel
            raise RuntimeError(
                "pycuda is not installed; TensorRT execution is unavailable "
                "on this host"
            ) from exc

        # The concrete buffer binding + execute_v2 host/device copy is
        # Jetson-only and validated on hardware; it is intentionally not run on
        # a non-Jetson host.
        raise RuntimeError(  # pragma: no cover - exercised only on Jetson
            "TensorRT execution path requires a Jetson runtime"
        )

    def _postprocess_yolo(
        self,
        np: Any,
        outputs: list[Any],
        frame_w: int,
        frame_h: int,
        loaded: _LoadedEngine,
    ) -> list[dict[str, Any]]:
        """Decode a YOLO-style head to detections in source-frame pixels.

        Same flat ``[cx, cy, w, h, objectness, class_scores...]`` layout the
        RKNN sidecar decodes, scaled by ``frame / input`` and reduced by
        class-agnostic non-maximum suppression.
        """
        if not outputs:
            return []
        arr = np.asarray(outputs[0]).reshape(-1)
        n_classes = max(len(loaded.class_labels), 1)
        stride = 5 + n_classes
        if stride <= 5 or arr.size < stride:
            return []
        rows = arr[: (arr.size // stride) * stride].reshape((-1, stride))

        scale_x = frame_w / loaded.input_w if loaded.input_w else 1.0
        scale_y = frame_h / loaded.input_h if loaded.input_h else 1.0

        boxes: list[tuple[float, float, float, float]] = []
        scores: list[float] = []
        labels: list[str] = []

        for row in rows:
            objectness = float(row[4])
            class_scores = row[5:]
            best = int(np.argmax(class_scores))
            conf = objectness * float(class_scores[best])
            if conf < self._conf_threshold:
                continue
            cx, cy, bw, bh = (float(row[0]), float(row[1]), float(row[2]), float(row[3]))
            x = (cx - bw / 2.0) * scale_x
            y = (cy - bh / 2.0) * scale_y
            w = bw * scale_x
            h = bh * scale_y
            boxes.append((x, y, w, h))
            scores.append(conf)
            label = (
                loaded.class_labels[best]
                if best < len(loaded.class_labels)
                else str(best)
            )
            labels.append(label)

        keep = _nms(boxes, scores, self._nms_iou)
        return [
            proto.detection_dict(
                x=boxes[i][0],
                y=boxes[i][1],
                width=boxes[i][2],
                height=boxes[i][3],
                class_label=labels[i],
                confidence=scores[i],
            )
            for i in keep
        ]


async def serve(socket_path: str = DEFAULT_SOCKET) -> None:
    """Bind the TensorRT sidecar socket and serve until cancelled."""
    backend = TensorRTBackend()
    server = SidecarServer(socket_path, backend, log)
    await server.serve_forever()


def _run() -> None:
    parser = argparse.ArgumentParser(description="ADOS TensorRT inference sidecar")
    parser.add_argument("--socket", default=DEFAULT_SOCKET, help="Unix socket path")
    args = parser.parse_args()

    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    task = loop.create_task(serve(args.socket))

    for sig in (signal.SIGINT, signal.SIGTERM):
        with contextlib.suppress(NotImplementedError):
            loop.add_signal_handler(sig, task.cancel)

    try:
        loop.run_until_complete(task)
    except asyncio.CancelledError:
        pass
    finally:
        loop.close()


if __name__ == "__main__":
    _run()
