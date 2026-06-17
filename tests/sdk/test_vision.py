"""Tests for the Python vision SDK surface.

Covers the wire contract (msgpack field names that must match the shared
frame-transport contract byte for byte so Python and Rust plugins read and
publish the same wire), the seqlock ring read path, the FakeVisionEngine
delivery (in-process and end-to-end through the real client over a file-backed
ring), pose/odometry frame building, and capability gating.
"""

from __future__ import annotations

import math
import struct

import msgpack
import pytest

from ados.plugins.errors import CapabilityDenied
from ados.sdk.testing import FakeVisionEngine
from ados.sdk.testing.stubs import FakeIpcClient
from ados.sdk.vision import (
    POSE_COVARIANCE_LEN,
    VIO_COMPONENT_ID,
    BoundingBox,
    Detection,
    DetectionBatch,
    Frame,
    FrameDescriptor,
    FrameFormat,
    ModelExecution,
    ModelKind,
    ModelMetadata,
    Odometry,
    Pose,
    RingLayout,
    VisionClient,
    read_slot,
    write_slot,
)

# ---------------------------------------------------------------------------
# Wire contract: field names match the shared frame-transport contract.
# ---------------------------------------------------------------------------


def _unpack(blob: bytes) -> dict:
    return msgpack.unpackb(blob, raw=False)


def test_frame_format_values_are_the_lowercase_wire_strings():
    assert FrameFormat.RGB24.value == "rgb24"
    assert FrameFormat.NV12.value == "nv12"
    assert FrameFormat.YUV420P.value == "yuv420p"


def test_frame_bytes_matches_format():
    assert FrameFormat.RGB24.frame_bytes(640, 480) == 640 * 480 * 3
    assert FrameFormat.NV12.frame_bytes(640, 480) == 640 * 480 * 3 // 2
    assert FrameFormat.YUV420P.frame_bytes(1280, 720) == 1280 * 720 * 3 // 2


def test_frame_descriptor_msgpack_field_names_match_rust():
    d = FrameDescriptor(
        camera_id="uvc-0",
        frame_id=42,
        ts_ms=1_700_000_000_000,
        width=640,
        height=480,
        format=FrameFormat.RGB24,
        shm_name="ados-vision-uvc-0",
        slot=3,
        seq=1234,
        byte_len=640 * 480 * 3,
    )
    raw = _unpack(d.to_msgpack())
    # Exactly the Rust FrameDescriptor field set, no more, no less.
    assert set(raw.keys()) == {
        "camera_id",
        "frame_id",
        "ts_ms",
        "width",
        "height",
        "format",
        "shm_name",
        "slot",
        "seq",
        "byte_len",
    }
    # format is the lowercase string the Rust serde(rename_all="lowercase") emits.
    assert raw["format"] == "rgb24"
    assert raw["frame_id"] == 42
    assert raw["byte_len"] == 640 * 480 * 3
    # Round-trips byte-identically.
    assert FrameDescriptor.from_msgpack(d.to_msgpack()) == d


def test_model_metadata_msgpack_field_names_match_rust():
    m = ModelMetadata(
        id="com.example.weeds",
        kind=ModelKind.DETECTION,
        execution=ModelExecution.ENGINE_RUN,
        input_width=640,
        input_height=480,
        input_format=FrameFormat.RGB24,
        output_classes=["weed", "crop"],
        model_path="/opt/ados/models/vision/weeds.onnx",
    )
    raw = _unpack(m.to_msgpack())
    assert set(raw.keys()) == {
        "id",
        "kind",
        "execution",
        "input_width",
        "input_height",
        "input_format",
        "output_classes",
        "model_path",
    }
    # Enum wire strings match the Rust renames.
    assert raw["kind"] == "detection"
    assert raw["execution"] == "engine_run"
    assert raw["input_format"] == "rgb24"
    assert raw["output_classes"] == ["weed", "crop"]
    assert ModelMetadata.from_msgpack(m.to_msgpack()) == m


def test_model_kind_and_execution_wire_strings():
    assert ModelKind.TRACKING.value == "tracking"
    assert ModelExecution.PLUGIN_SIDE.value == "plugin_side"


def test_detection_batch_msgpack_field_names_match_rust():
    b = DetectionBatch(
        model_id="com.example.weeds",
        camera_id="uvc-0",
        frame_id=7,
        ts_ms=1_700_000_000_000,
        detections=[
            Detection(
                bbox=BoundingBox(x=12.0, y=20.0, width=64.0, height=32.0),
                class_label="weed",
                confidence=0.87,
                track_id=None,
            )
        ],
    )
    raw = _unpack(b.to_msgpack())
    assert set(raw.keys()) == {
        "model_id",
        "camera_id",
        "frame_id",
        "ts_ms",
        "detections",
    }
    det = raw["detections"][0]
    assert set(det.keys()) == {
        "bbox",
        "class_label",
        "confidence",
        "track_id",
        "assoc_confidence",
        "lock_state",
    }
    assert set(det["bbox"].keys()) == {"x", "y", "width", "height"}
    assert det["class_label"] == "weed"
    assert det["track_id"] is None
    # The optional lock fields default-absent: present as keys but nil.
    assert det["assoc_confidence"] is None
    assert det["lock_state"] is None
    assert DetectionBatch.from_msgpack(b.to_msgpack()) == b


def test_detection_dict_shape_aligns_with_sidecar_protocol():
    # The inference sidecar emits the same bbox/detection field names; a
    # detection decoded from a sidecar-shaped mapping must round-trip.
    from ados.services.vision.sidecar_protocol import detection_dict

    raw = detection_dict(
        x=1.0, y=2.0, width=3.0, height=4.0,
        class_label="weed", confidence=0.9, track_id=7,
    )
    det = Detection.from_dict(raw)
    assert det == Detection(
        bbox=BoundingBox(1.0, 2.0, 3.0, 4.0),
        class_label="weed",
        confidence=0.9,
        track_id=7,
    )
    # The optional lock fields default-absent on both sides.
    assert det.assoc_confidence is None
    assert det.lock_state is None
    # And the round-trip back to a dict preserves the contract keys.
    assert det.to_dict() == raw

    # When the sidecar scores association, the lock fields round-trip too.
    raw_locked = detection_dict(
        x=1.0, y=2.0, width=3.0, height=4.0,
        class_label="weed", confidence=0.9, track_id=7,
        assoc_confidence=0.55, lock_state="uncertain",
    )
    det_locked = Detection.from_dict(raw_locked)
    assert det_locked.assoc_confidence == 0.55
    assert det_locked.lock_state == "uncertain"
    assert det_locked.to_dict() == raw_locked


# ---------------------------------------------------------------------------
# Ring layout + seqlock read path (ports the Rust framebus logic).
# ---------------------------------------------------------------------------


def test_ring_header_round_trips():
    layout = RingLayout.for_frame(4, 64, 48, FrameFormat.RGB24)
    region = bytearray(layout.total_len())
    layout.write_header(region)
    assert RingLayout.read_header(region) == layout


def test_ring_header_magic_and_version_bytes():
    layout = RingLayout.for_frame(2, 4, 4, FrameFormat.RGB24)
    region = bytearray(layout.total_len())
    layout.write_header(region)
    # "ADV1": magic 0x41445631 LE, version 1 LE.
    magic, version = struct.unpack("<IH", bytes(region[0:6]))
    assert magic == 0x41445631
    assert version == 1


def test_write_then_read_returns_the_frame():
    layout = RingLayout.for_frame(4, 8, 8, FrameFormat.RGB24)
    region = bytearray(layout.total_len())
    layout.write_header(region)
    frame = bytes(range(layout.slot_bytes % 256)) + bytes(
        layout.slot_bytes - (layout.slot_bytes % 256)
    )
    frame = frame[: layout.slot_bytes]
    seq = 9
    slot = seq % layout.slot_count
    write_slot(region, layout, slot, seq, frame)
    assert read_slot(region, layout, slot, seq) == frame


def test_stale_seq_reads_none():
    layout = RingLayout.for_frame(2, 4, 4, FrameFormat.RGB24)
    region = bytearray(layout.total_len())
    layout.write_header(region)
    write_slot(region, layout, 0, 10, bytes([1, 2, 3]))
    assert read_slot(region, layout, 0, 11) is None


def test_recycled_slot_is_detected_as_torn():
    layout = RingLayout.for_frame(1, 4, 4, FrameFormat.RGB24)
    region = bytearray(layout.total_len())
    layout.write_header(region)
    write_slot(region, layout, 0, 100, bytes([7, 7, 7, 7]))
    write_slot(region, layout, 0, 101, bytes([8, 8, 8, 8]))
    # A consumer still holding the old descriptor sees the new seq and drops.
    assert read_slot(region, layout, 0, 100) is None
    assert read_slot(region, layout, 0, 101) == bytes([8, 8, 8, 8])


def test_oversized_payload_rejected():
    layout = RingLayout.for_frame(2, 2, 2, FrameFormat.RGB24)
    region = bytearray(layout.total_len())
    layout.write_header(region)
    with pytest.raises(ValueError):
        write_slot(region, layout, 0, 1, bytes(layout.slot_bytes + 1))


def test_slot_out_of_range_rejected():
    layout = RingLayout.for_frame(2, 2, 2, FrameFormat.RGB24)
    region = bytearray(layout.total_len())
    layout.write_header(region)
    with pytest.raises(ValueError):
        write_slot(region, layout, 2, 1, bytes([0]))


def test_header_rejects_foreign_magic():
    region = bytearray(64)
    assert RingLayout.read_header(region) is None


# ---------------------------------------------------------------------------
# FakeVisionEngine: in-process delivery.
# ---------------------------------------------------------------------------


async def test_fake_engine_delivers_a_solid_frame_to_the_callback():
    engine = FakeVisionEngine("uvc-0", 4, 4, FrameFormat.RGB24)
    seen: list[Frame] = []
    engine.on_frame(lambda f: seen.append(f))

    engine.push_solid(0x42)
    assert await engine.deliver_all() == 1

    assert len(seen) == 1
    assert seen[0].descriptor.camera_id == "uvc-0"
    assert seen[0].descriptor.frame_id == 1
    assert len(seen[0].pixels) == engine.frame_bytes()
    assert all(b == 0x42 for b in seen[0].pixels)


async def test_fake_engine_delivers_frames_in_order():
    engine = FakeVisionEngine("uvc-0", 2, 2, FrameFormat.RGB24)
    ids: list[int] = []
    engine.on_frame(lambda f: ids.append(f.descriptor.frame_id))

    engine.push_solid(1)
    engine.push_solid(2)
    engine.push_solid(3)
    assert await engine.deliver_all() == 3
    assert ids == [1, 2, 3]


async def test_fake_engine_supports_async_callbacks():
    engine = FakeVisionEngine("uvc-0", 2, 2, FrameFormat.RGB24)
    seen: list[bytes] = []

    async def handler(frame: Frame) -> None:
        seen.append(frame.pixels)

    engine.on_frame(handler)
    engine.push_solid(0x09)
    assert await engine.deliver_all() == 1
    assert seen and all(b == 0x09 for b in seen[0])


async def test_fake_engine_push_dir_reads_raw_frames_in_name_order(tmp_path):
    (tmp_path / "b.bin").write_bytes(bytes([2] * 12))
    (tmp_path / "a.raw").write_bytes(bytes([1] * 12))
    (tmp_path / "skip.txt").write_bytes(b"not a frame")

    engine = FakeVisionEngine("uvc-0", 2, 2, FrameFormat.RGB24)  # 12-byte frames
    assert engine.push_dir(tmp_path) == 2

    first_bytes: list[int] = []
    engine.on_frame(lambda f: first_bytes.append(f.pixels[0]))
    await engine.deliver_all()
    # a.raw (value 1) sorts before b.bin (value 2).
    assert first_bytes == [1, 2]


def test_fake_engine_captures_published_detections():
    engine = FakeVisionEngine("uvc-0", 2, 2, FrameFormat.RGB24)
    batch = DetectionBatch(
        model_id="com.example.weeds",
        camera_id="uvc-0",
        frame_id=1,
        ts_ms=1,
        detections=[
            Detection(
                bbox=BoundingBox(0.0, 0.0, 1.0, 1.0),
                class_label="weed",
                confidence=0.5,
            )
        ],
    )
    engine.capture(batch)
    assert engine.captured_detections() == [batch]
    engine.clear_captured()
    assert engine.captured_detections() == []


# ---------------------------------------------------------------------------
# FakeVisionEngine: end-to-end through the real VisionClient + file-backed ring.
# ---------------------------------------------------------------------------


async def test_end_to_end_frame_reaches_plugin_handler_through_real_client():
    engine = FakeVisionEngine.with_shm_dir("uvc-0", 4, 4, FrameFormat.RGB24)
    try:
        seen: list[Frame] = []
        await engine.attach(lambda f: seen.append(f))

        engine.push_solid(0xAB)
        engine.push_solid(0xCD)
        assert await engine.deliver_all() == 2

        # The frames travelled the production path: descriptor over a
        # vision.deliver event, ring mapped read-only from a file, slot read
        # through the seqlock. The plugin handler receives correct pixels.
        assert [f.descriptor.frame_id for f in seen] == [1, 2]
        assert all(b == 0xAB for b in seen[0].pixels)
        assert all(b == 0xCD for b in seen[1].pixels)
        assert len(seen[0].pixels) == engine.frame_bytes()
    finally:
        engine.close()


async def test_end_to_end_camera_filter_drops_other_cameras():
    engine = FakeVisionEngine.with_shm_dir("uvc-0", 2, 2, FrameFormat.RGB24)
    try:
        seen: list[Frame] = []
        # Subscribe only to a different camera id; frames must be filtered.
        await engine.attach(lambda f: seen.append(f), camera_id="uvc-9")
        engine.push_solid(0x11)
        await engine.deliver_all()
        assert seen == []
    finally:
        engine.close()


# ---------------------------------------------------------------------------
# VisionClient RPC shaping + capability gating (over FakeIpcClient).
# ---------------------------------------------------------------------------


def _ipc(caps: set[str]) -> FakeIpcClient:
    return FakeIpcClient(plugin_id="com.example.vision", granted_capabilities=caps)


async def test_register_model_sends_blob_under_correct_cap():
    ipc = _ipc({"vision.model.register"})
    client = VisionClient(ipc)
    model = ModelMetadata(
        id="com.example.weeds",
        kind=ModelKind.DETECTION,
        execution=ModelExecution.PLUGIN_SIDE,
        input_width=320,
        input_height=320,
        input_format=FrameFormat.RGB24,
    )
    await client.register_model(model)
    method, args = ipc.requests[-1]
    assert method == "vision.register_model"
    # The blob is the model's named-field msgpack; decodes back to the model.
    assert ModelMetadata.from_msgpack(args["model"]) == model


async def test_register_model_denied_without_cap():
    client = VisionClient(_ipc(set()))
    model = ModelMetadata(
        id="m", kind=ModelKind.DETECTION, execution=ModelExecution.PLUGIN_SIDE,
        input_width=8, input_height=8, input_format=FrameFormat.RGB24,
    )
    with pytest.raises(CapabilityDenied):
        await client.register_model(model)


async def test_infer_passes_descriptor_and_decodes_detections():
    ipc = _ipc({"vision.model.register"})
    # Stage the engine's infer response: detections as a msgpack blob, matching
    # the wire the host returns.
    dets = [
        Detection(
            bbox=BoundingBox(1.0, 2.0, 3.0, 4.0),
            class_label="weed",
            confidence=0.9,
            track_id=7,
        ).to_dict()
    ]
    ipc.set_response(
        "vision.infer", {"detections": msgpack.packb(dets, use_bin_type=True)}
    )
    client = VisionClient(ipc)

    descriptor = FrameDescriptor(
        camera_id="uvc-0", frame_id=1, ts_ms=1, width=4, height=4,
        format=FrameFormat.RGB24, shm_name="ados-vision-uvc-0",
        slot=1, seq=1, byte_len=48,
    )
    frame = Frame(descriptor=descriptor, pixels=bytes(48))
    out = await client.infer("com.example.weeds", frame)

    method, args = ipc.requests[-1]
    assert method == "vision.infer"
    assert args["model_id"] == "com.example.weeds"
    assert FrameDescriptor.from_msgpack(args["descriptor"]) == descriptor
    assert out == [
        Detection(
            bbox=BoundingBox(1.0, 2.0, 3.0, 4.0),
            class_label="weed",
            confidence=0.9,
            track_id=7,
        )
    ]


async def test_infer_empty_response_is_no_detections():
    ipc = _ipc({"vision.model.register"})
    client = VisionClient(ipc)
    descriptor = FrameDescriptor(
        camera_id="uvc-0", frame_id=1, ts_ms=1, width=2, height=2,
        format=FrameFormat.RGB24, shm_name="r", slot=0, seq=1, byte_len=12,
    )
    assert await client.infer("m", Frame(descriptor, bytes(12))) == []


async def test_publish_detection_sends_batch_blob_under_correct_cap():
    ipc = _ipc({"vision.detection.publish"})
    client = VisionClient(ipc)
    batch = DetectionBatch(
        model_id="m", camera_id="uvc-0", frame_id=1, ts_ms=1,
        detections=[
            Detection(BoundingBox(0, 0, 1, 1), "weed", 0.5)
        ],
    )
    await client.publish_detection(batch)
    method, args = ipc.requests[-1]
    assert method == "vision.publish_detection"
    assert DetectionBatch.from_msgpack(args["batch"]) == batch


async def test_publish_one_builds_a_batch_from_the_frame():
    ipc = _ipc({"vision.detection.publish"})
    client = VisionClient(ipc)
    descriptor = FrameDescriptor(
        camera_id="uvc-3", frame_id=12, ts_ms=99, width=2, height=2,
        format=FrameFormat.RGB24, shm_name="r", slot=0, seq=12, byte_len=12,
    )
    det = Detection(BoundingBox(1, 1, 2, 2), "crop", 0.7)
    await client.publish_one("m", Frame(descriptor, bytes(12)), det)
    _, args = ipc.requests[-1]
    sent = DetectionBatch.from_msgpack(args["batch"])
    assert sent.camera_id == "uvc-3"
    assert sent.frame_id == 12
    assert sent.ts_ms == 99
    assert sent.detections == [det]


async def test_subscribe_frames_denied_without_cap():
    client = VisionClient(_ipc({"event.subscribe"}))
    with pytest.raises(CapabilityDenied):
        await client.subscribe_frames(lambda _f: None)


# ---------------------------------------------------------------------------
# Pose / odometry injection.
# ---------------------------------------------------------------------------


def test_pose_identity_builds_a_vision_position_estimate_frame():
    frame = Pose.identity(1_700_000_000_000).to_vision_position_estimate_frame()
    assert frame[0] == 0xFD  # MAVLink v2 magic.
    # comp id is the visual-odometry component.
    assert frame[6] == VIO_COMPONENT_ID
    # msg id 102 (VISION_POSITION_ESTIMATE), 3 LE bytes at offset 7..10.
    msg_id = int.from_bytes(frame[7:10], "little")
    assert msg_id == 102


def test_pose_quaternion_yaw_maps_to_euler_yaw():
    s = 1.0 / math.sqrt(2.0)  # 90 deg yaw about Z.
    pose = Pose(
        position=(1.0, 2.0, -3.0),
        orientation=(s, 0.0, 0.0, s),
        timestamp_us=42,
    )
    roll, pitch, yaw = pose.euler_rpy()
    assert abs(yaw - math.pi / 2.0) < 1e-5
    assert abs(roll) < 1e-5
    assert abs(pitch) < 1e-5


def test_pose_covariance_unknown_marker_is_nan():
    field = Pose.identity(1).covariance_field()
    assert len(field) == POSE_COVARIANCE_LEN
    assert math.isnan(field[0])


def test_odometry_builds_an_odometry_frame():
    odo = Odometry(
        pose=Pose(
            position=(5.0, 6.0, 7.0),
            orientation=(1.0, 0.0, 0.0, 0.0),
            timestamp_us=99,
            covariance=tuple([0.1] * POSE_COVARIANCE_LEN),
        ),
        linear_velocity=(0.5, -0.5, 0.0),
        angular_velocity=(0.01, 0.02, 0.03),
    )
    frame = odo.to_odometry_frame()
    assert frame[0] == 0xFD
    assert frame[6] == VIO_COMPONENT_ID
    msg_id = int.from_bytes(frame[7:10], "little")
    assert msg_id == 331  # ODOMETRY


async def test_inject_pose_sends_over_mavlink_path_under_vio_component():
    ipc = _ipc({"mavlink.write"})
    client = VisionClient(ipc)
    await client.inject_pose(Pose.identity(123))
    sent_bytes, comp = ipc.sent_mavlink[-1]
    assert comp == VIO_COMPONENT_ID
    assert sent_bytes[0] == 0xFD


async def test_register_vio_component_uses_vio_kind():
    ipc = _ipc({"mavlink.component.vio"})
    client = VisionClient(ipc)
    await client.register_vio_component()
    assert ipc.registered_components[-1] == (VIO_COMPONENT_ID, "vio")
