# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Decode tests for the ``yolox`` head: the undecoded YOLOX outputs, one
``[1, 5+nc, H, W]`` grid per stride, as the NPU person detector emits them.

The synthetic cases pin the cell-to-pixel arithmetic and the letterbox. The
model case runs the exported raw-heads ONNX file in ONNX Runtime and checks the
sidecar decode against an independent numpy port of YOLOX's own decoder; it
needs ``onnxruntime`` and ``ADOS_YOLOX_HEADS_ONNX`` (the file's path), and also
decodes every PNG/JPEG in ``ADOS_YOLOX_TEST_IMAGES`` when Pillow is present.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

np = pytest.importorskip("numpy", reason="requires the `vision` extra (numpy)")

from ados.services.vision import sidecar_protocol as proto  # noqa: E402
from ados.services.vision.yolo_decode import (  # noqa: E402
    LETTERBOX_FILL,
    Letterbox,
    decode_yolo_detections,
    letterbox_rgb24_nhwc,
    yolox_rows,
)

SIZE = 640
STRIDES = (8, 16, 32)


def _empty_heads(fill_obj: float = 0.0) -> list:
    """Raw heads with every cell scoring ``fill_obj`` objectness, zero person."""
    outs = []
    for s in STRIDES:
        g = SIZE // s
        t = np.zeros((1, 6, g, g), dtype=np.float32)
        t[0, 4] = fill_obj
        outs.append(t)
    return outs


def _plant(outs, stride, row, col, dx, dy, w_px, h_px, obj, person):
    k = STRIDES.index(stride)
    outs[k][0, :, row, col] = [dx, dy, np.log(w_px / stride), np.log(h_px / stride), obj, person]


def _decode(outs, *, frame=(SIZE, SIZE), fit=None, conf=0.25):
    return decode_yolo_detections(
        np,
        outs,
        frame_w=frame[0],
        frame_h=frame[1],
        input_w=SIZE,
        input_h=SIZE,
        class_labels=["person"],
        head="yolox",
        conf_threshold=conf,
        nms_iou=0.45,
        fit=fit,
    )


def test_a_cell_decodes_to_its_pixel_box_on_every_stride():
    outs = _empty_heads()
    # Stride 8, cell (row 10, col 20): centre ((0.5+20)*8, (0.25+10)*8) = (164, 82).
    _plant(outs, 8, 10, 20, 0.5, 0.25, 32.0, 64.0, 0.9, 0.9)
    # Stride 32, cell (row 3, col 15): centre (15.5*32, 3.5*32) = (496, 112).
    _plant(outs, 32, 3, 15, 0.5, 0.5, 96.0, 200.0, 0.8, 0.5)
    dets = sorted(_decode(outs), key=lambda d: -d["confidence"])
    assert len(dets) == 2
    a, b = (d["bbox"] for d in dets)
    assert (a["x"], a["y"], a["width"], a["height"]) == pytest.approx((148.0, 50.0, 32.0, 64.0))
    assert dets[0]["confidence"] == pytest.approx(0.81)
    assert dets[0]["class_label"] == "person"
    assert (b["x"], b["y"], b["width"], b["height"]) == pytest.approx((448.0, 12.0, 96.0, 200.0))
    assert dets[1]["confidence"] == pytest.approx(0.40)


def test_cells_flatten_row_major_with_the_column_as_x():
    outs = _empty_heads()
    _plant(outs, 16, 2, 7, 0.0, 0.0, 16.0, 16.0, 1.0, 1.0)
    rows = yolox_rows(np, outs, SIZE, SIZE)
    assert rows.shape == (80 * 80 + 40 * 40 + 20 * 20, 6)
    hit = 80 * 80 + 2 * 40 + 7
    assert rows[hit, :4] == pytest.approx([7 * 16.0, 2 * 16.0, 16.0, 16.0])
    assert rows[hit, 4:] == pytest.approx([1.0, 1.0])


def test_channels_last_heads_decode_the_same():
    outs = _empty_heads()
    _plant(outs, 8, 40, 3, 0.1, 0.9, 20.0, 50.0, 0.95, 0.9)
    nhwc = [o.transpose(0, 2, 3, 1) for o in outs]
    assert _decode(nhwc) == _decode(outs)


def test_a_head_of_the_wrong_shape_decodes_to_nothing():
    assert _decode([np.zeros((1, 6, 8400), dtype=np.float32)]) == []
    assert _decode([np.zeros((1, 3, 80, 80), dtype=np.float32)]) == []
    assert _decode([]) == []


def test_overlapping_cells_reduce_to_the_best_one():
    outs = _empty_heads()
    _plant(outs, 8, 30, 30, 0.5, 0.5, 60.0, 120.0, 0.9, 0.9)
    _plant(outs, 8, 30, 31, 0.0, 0.5, 60.0, 120.0, 0.8, 0.9)
    dets = _decode(outs)
    assert len(dets) == 1
    assert dets[0]["confidence"] == pytest.approx(0.81)


def test_letterbox_centres_the_frame_and_the_decode_maps_back():
    # A 1280x720 frame fits at scale 0.5 with 140 px bands above and below.
    frame = np.zeros((720, 1280, 3), dtype=np.uint8)
    frame[100:300, 200:400] = 255
    tensor, fit = letterbox_rgb24_nhwc(np, frame.tobytes(), 1280, 720, SIZE, SIZE)
    assert tensor.shape == (1, SIZE, SIZE, 3)
    assert fit == Letterbox(0.5, 0.0, 140.0)
    assert (tensor[0, :140] == LETTERBOX_FILL).all()
    assert (tensor[0, 500:] == LETTERBOX_FILL).all()
    assert (tensor[0, 190:290, 100:200] == 255).all()
    assert (tensor[0, 140:189] == 0).all()
    # The white block, as a detection at input (100..200, 190..290).
    outs = _empty_heads()
    _plant(outs, 8, 30, 18, 0.75, 0.0, 100.0, 100.0, 0.9, 0.9)  # centre (150, 240)
    (det,) = _decode(outs, frame=(1280, 720), fit=fit)
    b = det["bbox"]
    assert (b["x"], b["y"], b["width"], b["height"]) == pytest.approx((200.0, 100.0, 200.0, 200.0))


def test_letterbox_refuses_a_short_frame():
    with pytest.raises(ValueError):
        letterbox_rgb24_nhwc(np, b"\x00" * 10, 4, 4, SIZE, SIZE)


def test_rknn_infer_letterboxes_a_camera_frame_and_returns_frame_pixels(tmp_path, monkeypatch):
    from ados.services.vision.rknn_sidecar import RknnBackend

    class Runtime:
        fed: list = []

        def load_rknn(self, path):
            return 0

        def init_runtime(self):
            return 0

        def release(self):
            pass

        def inference(self, inputs):
            Runtime.fed.append(inputs[0].shape)
            outs = _empty_heads()
            _plant(outs, 8, 30, 18, 0.75, 0.0, 100.0, 100.0, 0.9, 0.9)
            return outs

    monkeypatch.setattr(RknnBackend, "_import_runtime", staticmethod(lambda: Runtime))
    model = tmp_path / "det.rknn"
    model.write_bytes(b"rknn")
    backend = RknnBackend()
    load = proto.LoadModelRequest("det", str(model), SIZE, SIZE, "rgb24", ["person"], "yolox")
    assert backend.load_model(load)["status"] == proto.STATUS_OK

    frame = bytes(1280 * 720 * 3)
    resp = backend.infer(proto.InferRequest("det", frame, 1280, 720, "rgb24"))
    assert resp["status"] == proto.STATUS_OK
    assert Runtime.fed == [(1, SIZE, SIZE, 3)]
    (det,) = resp["detections"]
    b = det["bbox"]
    assert (b["x"], b["y"], b["width"], b["height"]) == pytest.approx((200.0, 100.0, 200.0, 200.0))

    # A frame already at the input size goes in as it is.
    resp = backend.infer(proto.InferRequest("det", bytes(SIZE * SIZE * 3), SIZE, SIZE, "rgb24"))
    (det,) = resp["detections"]
    assert det["bbox"]["x"] == pytest.approx(100.0)
    assert det["bbox"]["y"] == pytest.approx(190.0)


# ---------------------------------------------------------------------------
# The exported model against an independent port of YOLOX's decoder.
# ---------------------------------------------------------------------------


def _reference_decode(outs, conf, iou):
    """YOLOX ``YOLOXHead.decode_outputs`` (flatten each level, concatenate,
    add the meshgrid, scale by the stride) then score gate and greedy NMS,
    written independently of the sidecar code. Returns ``(rows, dets)`` with
    ``dets`` as ``(x, y, w, h, score)`` in score order."""
    flat = np.concatenate([o.reshape(1, o.shape[1], -1) for o in outs], axis=2)[0].T
    grids, strides = [], []
    for o in outs:
        h, w = o.shape[-2:]
        yv, xv = np.meshgrid(np.arange(h), np.arange(w), indexing="ij")
        grids.append(np.stack((xv, yv), 2).reshape(-1, 2))
        strides.append(np.full((h * w, 1), SIZE // h))
    grid = np.concatenate(grids).astype(np.float64)
    stride = np.concatenate(strides).astype(np.float64)
    xy = (flat[:, :2] + grid) * stride
    wh = np.exp(flat[:, 2:4].astype(np.float64)) * stride
    rows = np.concatenate([xy, wh, flat[:, 4:]], axis=1)
    score = flat[:, 4] * flat[:, 5]
    idx = np.nonzero(score >= conf)[0]
    idx = idx[np.argsort(-score[idx], kind="stable")]
    x1, y1 = xy[:, 0] - wh[:, 0] / 2, xy[:, 1] - wh[:, 1] / 2
    x2, y2 = x1 + wh[:, 0], y1 + wh[:, 1]
    kept = []
    while idx.size:
        i, rest = idx[0], idx[1:]
        kept.append(i)
        iw = np.clip(np.minimum(x2[i], x2[rest]) - np.maximum(x1[i], x1[rest]), 0, None)
        ih = np.clip(np.minimum(y2[i], y2[rest]) - np.maximum(y1[i], y1[rest]), 0, None)
        inter = iw * ih
        union = wh[i, 0] * wh[i, 1] + wh[rest, 0] * wh[rest, 1] - inter
        idx = rest[inter / union < iou]
    dets = [(x1[i], y1[i], wh[i, 0], wh[i, 1], score[i]) for i in kept]
    return rows, dets


def _model_frames():
    rng = np.random.default_rng(3)
    ys, xs = np.mgrid[0:SIZE, 0:SIZE]
    gradient = np.stack([xs * 255 // SIZE, ys * 255 // SIZE, (xs + ys) * 255 // (2 * SIZE)], -1)
    blocks = ((xs // 40 + ys // 40) % 2)[..., None] * np.array([200, 120, 40]) + 30
    frames = [
        ("noise", rng.integers(0, 256, (SIZE, SIZE, 3), dtype=np.uint8)),
        ("gradient", gradient.astype(np.uint8)),
        ("blocks", blocks.astype(np.uint8)),
    ]
    image_dir = os.environ.get("ADOS_YOLOX_TEST_IMAGES")
    if image_dir:
        image = pytest.importorskip("PIL.Image")
        for path in sorted(Path(image_dir).iterdir()):
            if path.suffix.lower() in (".png", ".jpg", ".jpeg"):
                frames.append((path.name, np.asarray(image.open(path).convert("RGB"))))
    return frames


def test_the_exported_heads_decode_like_yoloxs_own_decoder():
    ort = pytest.importorskip("onnxruntime")
    onnx_path = os.environ.get("ADOS_YOLOX_HEADS_ONNX")
    if not onnx_path or not Path(onnx_path).is_file():
        pytest.skip("set ADOS_YOLOX_HEADS_ONNX to the raw-heads ONNX file")
    sess = ort.InferenceSession(onnx_path, providers=["CPUExecutionProvider"])
    name = sess.get_inputs()[0].name
    conf, iou = 0.05, 0.45
    for label, frame in _model_frames():
        h, w = frame.shape[:2]
        tensor, fit = letterbox_rgb24_nhwc(np, frame.tobytes(), w, h, SIZE, SIZE)
        chw = np.ascontiguousarray(tensor.transpose(0, 3, 1, 2), dtype=np.float32) / 255.0
        outs = sess.run(None, {name: chw})
        assert [o.shape for o in outs] == [(1, 6, SIZE // s, SIZE // s) for s in STRIDES]

        ref_rows, ref = _reference_decode(outs, conf, iou)
        got_rows = yolox_rows(np, outs, SIZE, SIZE)
        np.testing.assert_allclose(got_rows, ref_rows, rtol=1e-5, atol=1e-3, err_msg=label)

        got = decode_yolo_detections(
            np,
            outs,
            frame_w=SIZE,
            frame_h=SIZE,
            input_w=SIZE,
            input_h=SIZE,
            class_labels=["person"],
            head="yolox",
            conf_threshold=conf,
            nms_iou=iou,
        )
        assert len(got) == len(ref), label
        for d, (x, y, bw, bh, s) in zip(got, ref, strict=True):
            b = d["bbox"]
            assert (b["x"], b["y"], b["width"], b["height"]) == pytest.approx(
                (x, y, bw, bh), abs=1e-2
            ), label
            assert d["confidence"] == pytest.approx(s, abs=1e-5), label
        # The same detections in the frame's own pixels.
        mapped = decode_yolo_detections(
            np,
            outs,
            frame_w=w,
            frame_h=h,
            input_w=SIZE,
            input_h=SIZE,
            class_labels=["person"],
            head="yolox",
            conf_threshold=conf,
            nms_iou=iou,
            fit=fit,
        )
        for d, m in zip(got, mapped, strict=True):
            assert m["bbox"]["x"] == pytest.approx((d["bbox"]["x"] - fit.pad_x) / fit.scale)
            assert m["bbox"]["height"] == pytest.approx(d["bbox"]["height"] / fit.scale)
