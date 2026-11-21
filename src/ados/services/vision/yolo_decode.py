# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""YOLO detection-head decoding shared by the NPU sidecars (RKNN, Hailo).

``head`` selects the output layout:

* ``"yolov8"`` (the default): the transposed ``[1, 4+nc, anchors]`` head — four
  box rows ``(cx, cy, w, h)`` then one score row per class, no objectness. This
  is the ultralytics YOLOv8/v11 export.
* ``"yolov5"``: the legacy ``[1, anchors, 5+nc]`` head — per anchor
  ``[cx, cy, w, h, objectness, class_scores...]`` (YOLOv5/v7).
* ``"yolox"``: the undecoded YOLOX heads, one ``[1, 5+nc, H, W]`` output per
  stride (8, 16, 32), each cell ``[dx, dy, log w, log h, objectness,
  class_scores...]`` with the box offsets from the cell and the sizes in stride
  units. The stride of an output is the model input height over its ``H``. The
  cells are decoded to pixel boxes (``x = (dx + col) * s``, ``w = exp(log w) *
  s``) and then read as the ``yolov5`` layout. NPU compilers take the network
  in this form: the decode (a reshape, a concat and a transpose after the
  convolutions) is the part they cannot compile.

Boxes come out in the model's input resolution. They map back to the source
frame through ``fit`` (the letterbox the frame was fitted with) when given,
else by ``frame / input`` per axis (the frame squeezed into the input); they
pass a confidence gate and class-agnostic non-maximum suppression.
"""

from __future__ import annotations

from typing import Any, NamedTuple

from ados.services.vision import sidecar_protocol as proto

# The value a letterboxed input is padded with (the YOLO training pad).
LETTERBOX_FILL = 114

# Above this a log-size is no box (exp(15) strides is millions of pixels); the
# clamp only keeps exp from overflowing on a cell the confidence gate drops.
_MAX_LOG_SIZE = 15.0


class Letterbox(NamedTuple):
    """A frame fitted into the model input with its aspect kept and centred:
    input pixel = frame pixel * ``scale`` + pad."""

    scale: float
    pad_x: float
    pad_y: float


def letterbox_rgb24_nhwc(
    np: Any, frame: bytes, width: int, height: int, out_w: int, out_h: int
) -> tuple[Any, Letterbox]:
    """Fit an RGB24 frame into ``out_w`` x ``out_h`` with its aspect kept,
    centred and padded with :data:`LETTERBOX_FILL` (nearest sample), as a
    ``(1, out_h, out_w, 3)`` uint8 tensor plus the fit. The geometry is the
    Rust ONNX backend's, so both backends see a frame at the same place."""
    buf = np.frombuffer(frame, dtype=np.uint8)
    expected = width * height * 3
    if width <= 0 or height <= 0 or buf.size < expected:
        raise ValueError(f"rgb24 frame is {buf.size} bytes, expected {expected}")
    img = buf[:expected].reshape((height, width, 3))
    scale = min(out_w / width, out_h / height)
    fit = Letterbox(scale, (out_w - width * scale) / 2.0, (out_h - height * scale) / 2.0)
    c0 = min(int(np.floor(fit.pad_x + 0.5)), out_w)
    c1 = min(int(np.floor(fit.pad_x + width * scale + 0.5)), out_w)
    r0 = min(int(np.floor(fit.pad_y + 0.5)), out_h)
    r1 = min(int(np.floor(fit.pad_y + height * scale + 0.5)), out_h)
    xs = np.clip(((np.arange(c0, c1) + 0.5 - fit.pad_x) / scale).astype(np.int64), 0, width - 1)
    ys = np.clip(((np.arange(r0, r1) + 0.5 - fit.pad_y) / scale).astype(np.int64), 0, height - 1)
    out = np.full((1, out_h, out_w, 3), LETTERBOX_FILL, dtype=np.uint8)
    out[0, r0:r1, c0:c1] = img[ys][:, xs]
    return out, fit


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
    fit: Letterbox | None = None,
) -> list[dict[str, Any]]:
    """Decode a YOLO detection head (see the module doc for ``head``) to
    detections in source-frame pixels. Confidence is the best class score
    (``yolov8``) or ``objectness x best class score`` (``yolov5``, ``yolox``).
    A single-tensor head delivered as ``(anchors, features)`` is transposed
    automatically."""
    if not outputs:
        return []
    n_classes = max(len(class_labels), 1)
    kind = str(head).lower()
    if kind == "yolox":
        arr = yolox_rows(np, outputs, input_w, input_h)
    else:
        arr = np.asarray(outputs[0], dtype=np.float32)
        # Drop leading singleton batch dims: (1, F, A) -> (F, A).
        while arr.ndim > 2 and arr.shape[0] == 1:
            arr = arr[0]
    if arr is None or arr.ndim != 2:
        return []

    if kind in ("yolov5", "yolo5", "v5", "yolox"):
        centers, scores, classes = _decode_v5(np, arr, n_classes, conf_threshold)
    else:
        centers, scores, classes = _decode_v8(np, arr, n_classes, conf_threshold)

    if centers.shape[0] == 0:
        return []

    cx, cy, bw, bh = centers[:, 0], centers[:, 1], centers[:, 2], centers[:, 3]
    if fit is not None:
        xs = (cx - bw / 2.0 - fit.pad_x) / fit.scale
        ys = (cy - bh / 2.0 - fit.pad_y) / fit.scale
        ws = bw / fit.scale
        hs = bh / fit.scale
    else:
        scale_x = frame_w / input_w if input_w else 1.0
        scale_y = frame_h / input_h if input_h else 1.0
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


def yolox_rows(np: Any, outputs: list[Any], input_w: int, input_h: int) -> Any:
    """The per-stride YOLOX heads as one ``(cells, 5+nc)`` array of
    ``[cx, cy, w, h, objectness, class_scores...]`` in input pixels, the
    cells of each output row-major (the order YOLOX's own decoder uses), or
    ``None`` when an output is not a ``[1, 5+nc, H, W]`` grid of this input.
    An output delivered channels-last (``[1, H, W, 5+nc]``) is accepted."""
    rows = []
    feat = None
    for out in outputs:
        arr = np.asarray(out, dtype=np.float32)
        while arr.ndim > 3 and arr.shape[0] == 1:
            arr = arr[0]
        if arr.ndim != 3:
            return None
        if arr.shape[0] > arr.shape[2]:
            arr = arr.transpose(2, 0, 1)
        c, h, w = arr.shape
        if c < 6 or h == 0 or w == 0 or (feat is not None and c != feat):
            return None
        feat = c
        sx, sy = input_w / w, input_h / h
        cells = arr.reshape(c, h * w).T
        gy, gx = np.divmod(np.arange(h * w, dtype=np.float32), np.float32(w))
        sizes = np.exp(np.minimum(cells[:, 2:4], _MAX_LOG_SIZE))
        rows.append(
            np.concatenate(
                [
                    ((cells[:, 0] + gx) * sx)[:, None],
                    ((cells[:, 1] + gy) * sy)[:, None],
                    (sizes[:, 0] * sx)[:, None],
                    (sizes[:, 1] * sy)[:, None],
                    cells[:, 4:],
                ],
                axis=1,
            )
        )
    if not rows:
        return None
    return np.concatenate(rows, axis=0).astype(np.float32)


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
