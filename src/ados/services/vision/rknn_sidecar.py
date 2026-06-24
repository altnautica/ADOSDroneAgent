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

Postprocessing decodes a YOLO detection head whose layout is selected per model
by the ``head`` field of the load request:

* ``"yolov8"`` (the default): the transposed ``[1, 4+nc, anchors]`` head — four
  box rows ``(cx, cy, w, h)`` then one score row per class, with no objectness.
  This is the ultralytics YOLOv8/v11 export.
* ``"yolov5"``: the legacy ``[1, anchors, 5+nc]`` head — per anchor
  ``[cx, cy, w, h, objectness, class_scores...]`` (YOLOv5/v7).

Boxes are in the model's input resolution; they scale back to the source frame
by ``frame / input``, pass a confidence gate, and are reduced by class-agnostic
non-maximum suppression before they cross the wire. The decoder is orientation
robust: a head delivered as ``(anchors, features)`` is transposed automatically.
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
    head: str = "yolov8"


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
            head=req.head,
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

    def embed(self, req: proto.EmbedRequest) -> dict[str, Any]:
        """Run a resident re-id model over one pre-cropped box and return its
        embedding. The crop arrives already cropped + resized to the model's
        input (the engine does the crop so the ONNX and RKNN paths consume
        identical bytes); the converted ``.rknn`` folds the appearance model's
        normalization, so a raw uint8 crop is fed. The output tensor is flattened
        to a 1-D embedding; the engine L2-normalizes it."""
        model = self._models.get(req.model_id)
        if model is None:
            return proto.error_response(f"model not loaded: {req.model_id}")

        try:
            import numpy as np  # lazy: numpy only needed on the real embed path
        except ImportError as exc:  # pragma: no cover - board has numpy
            return proto.error_response(f"numpy unavailable: {exc}")

        try:
            # The crop reshapes to NHWC uint8 exactly like a detector frame.
            crop_frame = proto.InferRequest(
                model_id=req.model_id,
                frame=req.crop,
                width=req.crop_w,
                height=req.crop_h,
                format=req.format,
            )
            tensor = self._frame_to_input(np, crop_frame, model)
            outputs = model.runtime.inference(inputs=[tensor])
        except Exception as exc:  # pragma: no cover - depends on board wheel
            log.error("rknn_embed_failed", model=req.model_id, error=str(exc))
            return proto.error_response(f"rknn embed error: {exc}")

        if not outputs:
            return proto.error_response("rknn embed produced no output")
        embedding = np.asarray(outputs[0], dtype=np.float32).reshape(-1).tolist()
        return proto.embedding_response(embedding)

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
        """Decode this model's head to detections in source-frame pixels.

        Delegates to :func:`decode_yolo_detections`, which selects the
        ``yolov8`` (transposed, no objectness) or ``yolov5`` (legacy, with
        objectness) layout by ``model.head``.
        """
        return decode_yolo_detections(
            np,
            outputs,
            frame_w=frame_w,
            frame_h=frame_h,
            input_w=model.input_w,
            input_h=model.input_h,
            class_labels=model.class_labels,
            head=model.head,
            conf_threshold=self._conf_threshold,
            nms_iou=self._nms_iou,
        )


def decode_yolo_detections(
    np: Any,
    outputs: list[Any],
    *,
    frame_w: int,
    frame_h: int,
    input_w: int,
    input_h: int,
    class_labels: list[str],
    head: str,
    conf_threshold: float,
    nms_iou: float,
) -> list[dict[str, Any]]:
    """Decode a YOLO detection head to detections in source-frame pixels.

    ``head`` selects the output-tensor layout:

    * ``"yolov8"`` (default): the transposed ``[1, 4+nc, anchors]`` head — four
      box rows ``(cx, cy, w, h)`` then one score row per class, no objectness.
    * ``"yolov5"``: the legacy ``[1, anchors, 5+nc]`` head — per anchor
      ``[cx, cy, w, h, objectness, class_scores...]``.

    Box coordinates are in the model's input resolution and scale to the source
    frame by ``frame / input``. Confidence is the best class score (v8) or
    ``objectness x best class score`` (v5). Boxes pass class-agnostic NMS. The
    decoder transposes a head delivered as ``(anchors, features)`` automatically.
    """
    if not outputs:
        return []
    arr = np.asarray(outputs[0], dtype=np.float32)
    # Drop leading singleton batch dims: (1, F, A) -> (F, A).
    while arr.ndim > 2 and arr.shape[0] == 1:
        arr = arr[0]
    if arr.ndim != 2:
        return []
    n_classes = max(len(class_labels), 1)

    if str(head).lower() in ("yolov5", "yolo5", "v5"):
        centers, scores, classes = _decode_v5(np, arr, n_classes, conf_threshold)
    else:
        centers, scores, classes = _decode_v8(np, arr, n_classes, conf_threshold)

    if centers.shape[0] == 0:
        return []

    scale_x = frame_w / input_w if input_w else 1.0
    scale_y = frame_h / input_h if input_h else 1.0
    cx, cy, bw, bh = centers[:, 0], centers[:, 1], centers[:, 2], centers[:, 3]
    xs = (cx - bw / 2.0) * scale_x
    ys = (cy - bh / 2.0) * scale_y
    ws = bw * scale_x
    hs = bh * scale_y

    boxes = [
        (float(xs[i]), float(ys[i]), float(ws[i]), float(hs[i]))
        for i in range(xs.shape[0])
    ]
    score_list = [float(s) for s in scores]
    keep = _nms(boxes, score_list, nms_iou)
    out: list[dict[str, Any]] = []
    for i in keep:
        ci = int(classes[i])
        label = class_labels[ci] if ci < len(class_labels) else str(ci)
        out.append(
            proto.detection_dict(
                x=boxes[i][0],
                y=boxes[i][1],
                width=boxes[i][2],
                height=boxes[i][3],
                class_label=label,
                confidence=score_list[i],
            )
        )
    return out


def _empty_decode(np: Any) -> tuple[Any, Any, Any]:
    """The no-detection result shape: zero-row centers/scores/classes arrays."""
    return (
        np.empty((0, 4), dtype=np.float32),
        np.empty((0,), dtype=np.float32),
        np.empty((0,), dtype=np.int64),
    )


def _decode_v8(np: Any, arr: Any, n_classes: int, conf_threshold: float) -> tuple[Any, Any, Any]:
    """Decode the transposed YOLOv8 head ``(4+nc, anchors)`` (no objectness)."""
    feat_len = 4 + n_classes
    if arr.shape[0] == feat_len:
        feat = arr
    elif arr.shape[1] == feat_len:
        feat = arr.T
    else:
        # Unknown class count: take the short axis as the feature axis.
        feat = arr if arr.shape[0] <= arr.shape[1] else arr.T
    if feat.shape[0] < 5:
        return _empty_decode(np)
    cls = feat[4 : 4 + n_classes, :]
    if cls.shape[0] == 0:
        return _empty_decode(np)
    best = cls.argmax(axis=0)
    conf = cls.max(axis=0)
    mask = conf >= conf_threshold
    centers = feat[:4, :].T[mask]
    return (
        centers.astype(np.float32),
        conf[mask].astype(np.float32),
        best[mask].astype(np.int64),
    )


def _decode_v5(np: Any, arr: Any, n_classes: int, conf_threshold: float) -> tuple[Any, Any, Any]:
    """Decode the legacy YOLOv5 head ``(anchors, 5+nc)`` (with objectness)."""
    row_len = 5 + n_classes
    if arr.shape[1] == row_len:
        rows = arr
    elif arr.shape[0] == row_len:
        rows = arr.T
    else:
        # Unknown class count: take the long axis as the anchor axis.
        rows = arr if arr.shape[0] >= arr.shape[1] else arr.T
    if rows.shape[1] < 6:
        return _empty_decode(np)
    obj = rows[:, 4]
    cls = rows[:, 5 : 5 + n_classes]
    if cls.shape[1] == 0:
        return _empty_decode(np)
    best = cls.argmax(axis=1)
    conf = obj * cls.max(axis=1)
    mask = conf >= conf_threshold
    centers = rows[mask, :4]
    return (
        centers.astype(np.float32),
        conf[mask].astype(np.float32),
        best[mask].astype(np.int64),
    )


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
