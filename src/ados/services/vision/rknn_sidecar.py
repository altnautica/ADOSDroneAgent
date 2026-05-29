# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""NPU inference sidecar for RKNN models (RK3588/RK3582/RK3576).

The Rust vision engine cannot link rknn-toolkit-lite2: it ships as a
proprietary Python-only wheel built against the Rockchip NPU runtime. This
module is the Python side that owns it. It listens on
``/run/ados/vision-rknn.sock`` and answers ``load_model`` / ``infer`` requests
in the :mod:`ados.services.vision.sidecar_protocol` shape, returning detections
already in the Rust ``Detection`` field layout.

rknn-toolkit-lite2 is imported lazily inside :class:`RknnBackend`. When the
wheel is absent (a dev laptop, a board without the NPU runtime), ``load_model``
returns an ``error`` response and the engine falls back to a Rust-side
ONNX Runtime path; the sidecar process itself stays up and serving.

Run as ``python -m ados.services.vision.rknn_sidecar``.

Postprocessing assumes a YOLO-style detection head: a flat tensor of
``[x, y, w, h, objectness, class_0..class_n]`` rows in input-image pixel space.
Boxes are decoded, scaled back to the source frame, confidence-thresholded, and
reduced by non-maximum suppression before they cross the wire.
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
from ados.services.vision.sidecar_protocol import SidecarServer

log = get_logger("vision-rknn")

DEFAULT_SOCKET = "/run/ados/vision-rknn.sock"

# Default YOLO-style postprocessing thresholds.
DEFAULT_CONF_THRESHOLD = 0.25
DEFAULT_NMS_IOU = 0.45


@dataclass
class _LoadedModel:
    """A resident RKNN runtime plus the metadata needed to label and scale its
    output back onto the source frame."""

    runtime: Any
    input_w: int
    input_h: int
    fmt: str
    class_labels: list[str]


class RknnBackend:
    """Loads ``.rknn`` models with rknn-toolkit-lite2 and runs inference.

    The toolkit import is deferred to :meth:`load_model` so the sidecar starts
    on any host. A missing wheel surfaces as an error response, not a crash.
    """

    def __init__(
        self,
        conf_threshold: float = DEFAULT_CONF_THRESHOLD,
        nms_iou: float = DEFAULT_NMS_IOU,
    ) -> None:
        self._models: dict[str, _LoadedModel] = {}
        self._conf_threshold = conf_threshold
        self._nms_iou = nms_iou

    @staticmethod
    def _import_runtime() -> Any:
        """Import the RKNN lite runtime class, or raise a clear error."""
        try:
            from rknnlite.api import RKNNLite  # type: ignore[import-not-found]
        except ImportError as exc:  # pragma: no cover - depends on board wheel
            raise RuntimeError(
                "rknn-toolkit-lite2 is not installed; RKNN inference is "
                "unavailable on this host"
            ) from exc
        return RKNNLite

    def load_model(self, req: proto.LoadModelRequest) -> dict[str, Any]:
        path = Path(req.path)
        if not path.is_file():
            return proto.error_response(f"model file not found: {req.path}")

        try:
            rknn_lite_cls = self._import_runtime()
        except RuntimeError as exc:
            return proto.error_response(str(exc))

        try:
            runtime = rknn_lite_cls()
            ret = runtime.load_rknn(str(path))
            if ret != 0:
                return proto.error_response(f"load_rknn failed (code {ret}) for {req.path}")
            # Auto target picks the NPU core layout for the running SoC.
            ret = runtime.init_runtime()
            if ret != 0:
                return proto.error_response(f"init_runtime failed (code {ret})")
        except Exception as exc:  # pragma: no cover - depends on board wheel
            log.error("rknn_load_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn load error: {exc}")

        self._models[req.model_id] = _LoadedModel(
            runtime=runtime,
            input_w=req.input_w,
            input_h=req.input_h,
            fmt=req.format,
            class_labels=req.class_labels,
        )
        log.info("rknn_model_loaded", model=req.model_id, path=req.path)
        return proto.ok_response()

    def infer(self, req: proto.InferRequest) -> dict[str, Any]:
        model = self._models.get(req.model_id)
        if model is None:
            return proto.error_response(f"model not loaded: {req.model_id}")

        try:
            import numpy as np  # lazy: numpy only needed on the real infer path
        except ImportError as exc:  # pragma: no cover - board has numpy
            return proto.error_response(f"numpy unavailable: {exc}")

        try:
            tensor = self._frame_to_input(np, req, model)
            outputs = model.runtime.inference(inputs=[tensor])
        except Exception as exc:  # pragma: no cover - depends on board wheel
            log.error("rknn_infer_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn inference error: {exc}")

        detections = self._postprocess_yolo(
            np, outputs, req.width, req.height, model
        )
        return proto.ok_response(detections)

    @staticmethod
    def _frame_to_input(np: Any, req: proto.InferRequest, model: _LoadedModel) -> Any:
        """Reshape a raw RGB24 frame into an NHWC uint8 tensor.

        Only ``rgb24`` is reshaped here; NV12/YUV420p frames are passed through
        as-is for runtimes that accept the planar buffer, which keeps this path
        free of a colour-conversion dependency.
        """
        buf = np.frombuffer(req.frame, dtype=np.uint8)
        if model.fmt == "rgb24":
            expected = req.width * req.height * 3
            if buf.size < expected:
                raise ValueError(
                    f"rgb24 frame is {buf.size} bytes, expected {expected}"
                )
            return buf[:expected].reshape((1, req.height, req.width, 3))
        return buf.reshape((1, -1))

    def _postprocess_yolo(
        self,
        np: Any,
        outputs: list[Any],
        frame_w: int,
        frame_h: int,
        model: _LoadedModel,
    ) -> list[dict[str, Any]]:
        """Decode a YOLO-style head to detections in source-frame pixels.

        The first output tensor is flattened to rows of
        ``[cx, cy, w, h, objectness, class_scores...]``. Coordinates are in the
        model's input resolution, so they scale by ``frame / input``. After a
        confidence gate (objectness x best class score) the boxes pass through
        class-agnostic non-maximum suppression.
        """
        if not outputs:
            return []
        arr = np.asarray(outputs[0]).reshape(-1)
        n_classes = max(len(model.class_labels), 1)
        stride = 5 + n_classes
        if stride <= 5 or arr.size < stride:
            return []
        rows = arr[: (arr.size // stride) * stride].reshape((-1, stride))

        scale_x = frame_w / model.input_w if model.input_w else 1.0
        scale_y = frame_h / model.input_h if model.input_h else 1.0

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
                model.class_labels[best]
                if best < len(model.class_labels)
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


def _iou(a: tuple[float, float, float, float], b: tuple[float, float, float, float]) -> float:
    """Intersection-over-union of two (x, y, w, h) boxes."""
    ax2, ay2 = a[0] + a[2], a[1] + a[3]
    bx2, by2 = b[0] + b[2], b[1] + b[3]
    ix1, iy1 = max(a[0], b[0]), max(a[1], b[1])
    ix2, iy2 = min(ax2, bx2), min(ay2, by2)
    iw, ih = max(0.0, ix2 - ix1), max(0.0, iy2 - iy1)
    inter = iw * ih
    if inter <= 0.0:
        return 0.0
    union = a[2] * a[3] + b[2] * b[3] - inter
    return inter / union if union > 0.0 else 0.0


def _nms(
    boxes: list[tuple[float, float, float, float]],
    scores: list[float],
    iou_threshold: float,
) -> list[int]:
    """Class-agnostic greedy non-maximum suppression. Returns kept indices."""
    order = sorted(range(len(boxes)), key=lambda i: scores[i], reverse=True)
    kept: list[int] = []
    while order:
        best = order.pop(0)
        kept.append(best)
        order = [i for i in order if _iou(boxes[best], boxes[i]) < iou_threshold]
    return kept


async def serve(socket_path: str = DEFAULT_SOCKET) -> None:
    """Bind the RKNN sidecar socket and serve until cancelled."""
    backend = RknnBackend()
    server = SidecarServer(socket_path, backend, log)
    await server.serve_forever()


def _run() -> None:
    parser = argparse.ArgumentParser(description="ADOS RKNN inference sidecar")
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
