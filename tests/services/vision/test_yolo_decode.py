# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Decode tests for the YOLO detection-head postprocessing.

The sidecar must decode the transposed YOLOv8 head ``[1, 4+nc, anchors]`` (box
rows then per-class score rows, no objectness) — NOT the legacy YOLOv5 head
``[1, anchors, 5+nc]`` (with an objectness column). A v8 model decoded as v5
shifts every field by one and reads the first class score as objectness, so this
covers both layouts on synthetic tensors with known boxes.
"""

from __future__ import annotations

import numpy as np
import pytest

from ados.services.vision.rknn_sidecar import decode_yolo_detections

LABELS = ["drone", "person"]


def _decode(outputs, head, *, frame=640, model=640, conf=0.25, iou=0.45):
    return decode_yolo_detections(
        np,
        outputs,
        frame_w=frame,
        frame_h=frame,
        input_w=model,
        input_h=model,
        class_labels=LABELS,
        head=head,
        conf_threshold=conf,
        nms_iou=iou,
    )


def _yolov8_tensor():
    # (1, 4+nc, anchors) = (1, 6, 3). Columns are anchors; rows are
    # cx, cy, w, h, score(drone), score(person).
    feat = np.array(
        [
            [100.0, 300.0, 10.0],  # cx
            [100.0, 300.0, 10.0],  # cy
            [40.0, 20.0, 5.0],  # w
            [40.0, 20.0, 5.0],  # h
            [0.90, 0.10, 0.05],  # drone score
            [0.10, 0.80, 0.02],  # person score
        ],
        dtype=np.float32,
    )
    return feat.reshape((1, 6, 3))


def test_yolov8_decodes_two_boxes_with_no_objectness():
    dets = _decode([_yolov8_tensor()], "yolov8")
    assert len(dets) == 2
    by_label = {d["class_label"]: d for d in dets}
    assert set(by_label) == {"drone", "person"}
    # cx=100,w=40 -> x = 100 - 20 = 80, width 40 (scale 1).
    drone = by_label["drone"]
    assert drone["bbox"]["x"] == pytest.approx(80.0)
    assert drone["bbox"]["y"] == pytest.approx(80.0)
    assert drone["bbox"]["width"] == pytest.approx(40.0)
    assert drone["confidence"] == pytest.approx(0.90)
    person = by_label["person"]
    assert person["bbox"]["x"] == pytest.approx(290.0)
    assert person["confidence"] == pytest.approx(0.80)


def test_yolov8_is_orientation_robust():
    # A head delivered as (1, anchors, features) must transpose to the same boxes.
    transposed = np.transpose(_yolov8_tensor(), (0, 2, 1))  # (1, 3, 6)
    dets = _decode([transposed], "yolov8")
    assert len(dets) == 2
    assert {d["class_label"] for d in dets} == {"drone", "person"}


def test_yolov8_misread_as_yolov5_does_not_match_the_correct_decode():
    # The bug this guards: decoding a v8 tensor with the v5 layout gives a
    # different (wrong) result. The v8 path is the contract; this proves the two
    # layouts are not interchangeable, so the head field matters.
    t = _yolov8_tensor()
    v8 = _decode([t], "yolov8")
    v5 = _decode([t], "yolov5")
    assert v8 != v5


def test_yolov8_threshold_gate_drops_low_confidence():
    # With a 0.95 gate, both boxes (0.90 / 0.80) fall away.
    dets = _decode([_yolov8_tensor()], "yolov8", conf=0.95)
    assert dets == []


def test_yolov5_decodes_with_objectness():
    # (1, anchors, 5+nc) = (1, 3, 7): cx,cy,w,h,obj,score0,score1.
    rows = np.array(
        [
            [100.0, 100.0, 40.0, 40.0, 0.90, 0.95, 0.10],  # conf 0.855 -> drone
            [300.0, 300.0, 20.0, 20.0, 0.80, 0.10, 0.90],  # conf 0.720 -> person
            [10.0, 10.0, 5.0, 5.0, 0.10, 0.50, 0.50],  # conf 0.05 -> dropped
        ],
        dtype=np.float32,
    ).reshape((1, 3, 7))
    dets = _decode([rows], "yolov5")
    assert len(dets) == 2
    by_label = {d["class_label"]: d for d in dets}
    assert by_label["drone"]["confidence"] == pytest.approx(0.855, abs=1e-4)
    assert by_label["person"]["confidence"] == pytest.approx(0.720, abs=1e-4)


def test_nms_collapses_overlapping_same_class_boxes():
    # Two near-identical drone boxes; NMS keeps the higher-confidence one.
    feat = np.array(
        [
            [100.0, 102.0],  # cx
            [100.0, 101.0],  # cy
            [40.0, 40.0],  # w
            [40.0, 40.0],  # h
            [0.90, 0.70],  # drone
            [0.01, 0.01],  # person
        ],
        dtype=np.float32,
    ).reshape((1, 6, 2))
    dets = _decode([feat], "yolov8")
    assert len(dets) == 1
    assert dets[0]["confidence"] == pytest.approx(0.90)


def test_empty_outputs_decode_to_nothing():
    assert _decode([], "yolov8") == []
    assert _decode([np.zeros((1, 6, 0), dtype=np.float32)], "yolov8") == []
