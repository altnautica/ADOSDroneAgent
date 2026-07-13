"""Tests for the Python compute-offload SDK facade (``ctx.compute``).

Covers the job interface a plugin uses (register dataset, submit, read status,
read outputs, cancel), that each RPC is tagged with the capability the host
enforces, and that a missing capability is denied.
"""

from __future__ import annotations

import asyncio

import pytest

from ados.plugins.errors import CapabilityDenied
from ados.sdk.compute import ComputeClient, JobStatus, Submission
from ados.sdk.offload import ExecutionTier
from ados.sdk.testing.stubs import FakeIpcClient
from ados.sdk.vision import DetectionBatch


def _ipc(caps: set[str]) -> FakeIpcClient:
    return FakeIpcClient(plugin_id="test-plugin", granted_capabilities=caps)


async def test_submit_and_read_a_job_through_the_facade():
    ipc = _ipc(
        {"compute.dataset.write", "compute.job.submit", "compute.job.read"}
    )
    ipc.set_response("compute.dataset.write", {"id": "ds-1"})
    ipc.set_response("compute.job.submit", {"job_id": "job-1", "state": "queued"})
    # The node's job record names its id `id` (not `job_id`).
    ipc.set_response(
        "compute.job.read",
        {"id": "job-1", "state": "completed", "progress": 1.0,
         "result_ref": "mock://splat/job-1"},
    )
    ipc.set_response(
        "compute.job.outputs",
        {"outputs": [{"id": "o1", "kind": "splat", "uri": "mock://splat/job-1"}]},
    )

    compute = ComputeClient(ipc)
    assert await compute.write_dataset("bag", {"cameras": 1}) == "ds-1"

    sub = await compute.submit_job("reconstruct", dataset_id="ds-1")
    assert sub == Submission(job_id="job-1", state="queued")

    status = await compute.read_job("job-1")
    assert isinstance(status, JobStatus)
    assert status.job_id == "job-1"  # read from the record's `id`
    assert status.state == "completed"
    assert status.progress == 1.0
    assert status.result_ref == "mock://splat/job-1"

    outputs = await compute.job_outputs("job-1")
    assert outputs[0]["kind"] == "splat"

    # Each call was tagged with the right RPC method (the host gates on the cap).
    methods = [m for m, _ in ipc.requests]
    assert methods == [
        "compute.dataset.write",
        "compute.job.submit",
        "compute.job.read",
        "compute.job.outputs",
    ]


async def test_a_perception_offload_carries_its_frame_in_params():
    ipc = _ipc({"compute.job.submit"})
    ipc.set_response("compute.job.submit", {"job_id": "off-1", "state": "queued"})
    compute = ComputeClient(ipc)
    frame = {"camera_id": "front", "width": 640, "height": 480, "ts_ms": 1}
    await compute.submit_job("perception_offload", params={"frame": frame})
    _, args = ipc.requests[0]
    assert args["kind"] == "perception_offload"
    assert args["dataset_id"] is None
    assert args["params"]["frame"] == frame


async def test_submit_without_the_capability_is_denied():
    ipc = _ipc({"compute.job.read"})  # read only, no submit
    compute = ComputeClient(ipc)
    with pytest.raises(CapabilityDenied):
        await compute.submit_job("reconstruct", dataset_id="ds-1")
    # The denied call never reached the wire.
    assert ipc.requests == []


async def test_an_unknown_job_kind_is_rejected_before_the_wire():
    ipc = _ipc({"compute.job.submit"})
    compute = ComputeClient(ipc)
    with pytest.raises(ValueError):
        await compute.submit_job("not-a-kind")
    assert ipc.requests == []


async def test_cancel_returns_the_node_verdict():
    ipc = _ipc({"compute.job.submit"})
    ipc.set_response("compute.job.cancel", {"cancelled": True})
    compute = ComputeClient(ipc)
    assert await compute.cancel_job("job-1") is True


async def test_cancel_without_the_submit_cap_is_denied():
    # Cancel is gated on compute.job.submit (the submitter cancels); read alone
    # must not authorize it.
    ipc = _ipc({"compute.job.read"})
    compute = ComputeClient(ipc)
    with pytest.raises(CapabilityDenied):
        await compute.cancel_job("job-1")
    assert ipc.requests == []


async def test_slam_offload_is_a_valid_submit_kind():
    ipc = _ipc({"compute.job.submit"})
    ipc.set_response("compute.job.submit", {"job_id": "slam-1", "state": "queued"})
    compute = ComputeClient(ipc)
    sub = await compute.submit_job("slam_offload", params={"frame": {"camera_id": "front"}})
    assert sub.job_id == "slam-1"
    method, args = ipc.requests[0]
    assert method == "compute.job.submit"
    assert args["kind"] == "slam_offload"


async def test_read_job_tolerates_a_malformed_node_reply():
    ipc = _ipc({"compute.job.read"})
    # progress as a string, result_ref / error absent.
    ipc.set_response("compute.job.read", {"id": "job-1", "state": "running", "progress": "nan"})
    compute = ComputeClient(ipc)
    status = await compute.read_job("job-1")
    assert status.progress == 0.0  # non-numeric guarded to 0.0
    assert status.result_ref is None
    assert status.error is None


async def test_job_outputs_tolerates_a_non_list_and_filters_non_dicts():
    ipc = _ipc({"compute.job.read"})
    compute = ComputeClient(ipc)
    # A non-list outputs value -> [].
    ipc.set_response("compute.job.outputs", {"outputs": "oops"})
    assert await compute.job_outputs("job-1") == []
    # Non-dict items are filtered out.
    ipc.set_response(
        "compute.job.outputs",
        {"outputs": [{"id": "o1", "kind": "splat"}, "garbage", 42]},
    )
    outputs = await compute.job_outputs("job-1")
    assert outputs == [{"id": "o1", "kind": "splat"}]


async def test_write_dataset_raises_when_the_node_returns_no_id():
    ipc = _ipc({"compute.dataset.write"})
    ipc.set_response("compute.dataset.write", {})  # successful-but-empty reply
    compute = ComputeClient(ipc)
    with pytest.raises(RuntimeError):
        await compute.write_dataset("bag")


# -------------------------------------------------------------------------
# Streaming perception-offload sessions (ctx.compute.open_stream).
# -------------------------------------------------------------------------


def _batch(camera_id: str = "front", frame_id: int = 1) -> DetectionBatch:
    return DetectionBatch(
        model_id="offload", camera_id=camera_id, frame_id=frame_id, ts_ms=frame_id
    )


async def test_open_stream_offload_async_iterates_returned_batches():
    # Opening needs compute.stream.open; iterating the returned batches rides the
    # shared detection bus, so it also needs vision.detection.subscribe.
    ipc = _ipc({"compute.stream.open", "vision.detection.subscribe"})
    ipc.set_response(
        "compute.stream.open",
        {
            "execution": "offload",
            "opened": True,
            "session_id": "s1",
            "camera_id": "front",
            "source": "rtsp://10.0.0.2:8554/main",
            "node": "http://10.0.0.9:8092",
        },
    )
    compute = ComputeClient(ipc)
    session = await compute.open_stream(camera_id="front", execution=ExecutionTier.AUTO)
    assert session.execution is ExecutionTier.OFFLOAD
    assert session.opened is True
    assert session.session_id == "s1"
    assert session.source == "rtsp://10.0.0.2:8554/main"

    # The open request went out tagged with the open cap.
    assert ipc.requests[0][0] == "compute.stream.open"
    assert ipc.requests[0][1]["execution"] == "auto"

    # `async for` arms the detection subscription lazily on first iteration, then
    # yields each delivered batch (the same DetectionBatch the engine publishes).
    consumer = asyncio.ensure_future(session.__anext__())
    for _ in range(5):  # let the subscription arm + the getter park
        await asyncio.sleep(0)
    # The subscribe RPC was sent (arming the host push) under the subscribe cap.
    assert any(m == "vision.subscribe_detections" for m, _ in ipc.requests)
    await ipc.deliver_detection({"batch": _batch().to_msgpack(), "timestamp_ms": 1})
    got = await asyncio.wait_for(consumer, timeout=1.0)
    assert isinstance(got, DetectionBatch)
    assert got.camera_id == "front"
    assert got.frame_id == 1

    await session.close()


async def test_open_stream_auto_resolves_offload_when_npu_less_paired_local_otherwise():
    # AUTO does not reimplement the tier decision: it sends the intent and the
    # host reports the resolved tier. On an NPU-less/paired signal the host
    # resolves to offload (opened); otherwise to local (nothing started).
    ipc = _ipc({"compute.stream.open"})
    compute = ComputeClient(ipc)

    # NPU-less + paired node -> the host resolves offload.
    ipc.set_response(
        "compute.stream.open",
        {"execution": "offload", "opened": True, "session_id": "s-off", "camera_id": "front"},
    )
    offloaded = await compute.open_stream(execution=ExecutionTier.AUTO)
    assert offloaded.execution is ExecutionTier.OFFLOAD
    assert offloaded.opened is True

    # An accelerator / no node -> the host resolves local, starts no session.
    ipc.set_response(
        "compute.stream.open",
        {"execution": "local", "opened": False, "session_id": "s-loc", "camera_id": "front"},
    )
    local = await compute.open_stream(execution=ExecutionTier.AUTO)
    assert local.execution is ExecutionTier.LOCAL
    assert local.opened is False


async def test_open_stream_health_reads_the_session_registry():
    ipc = _ipc({"compute.stream.open"})
    ipc.set_response(
        "compute.stream.open",
        {"execution": "offload", "opened": True, "session_id": "s1", "camera_id": "front"},
    )
    ipc.set_response(
        "compute.stream.health",
        {"id": "s1", "state": "live", "frames_processed": 12, "found": True},
    )
    compute = ComputeClient(ipc)
    session = await compute.open_stream(execution=ExecutionTier.OFFLOAD)
    health = await session.health()
    assert health["state"] == "live"
    assert health["frames_processed"] == 12
    # Health is gated on the open cap (the opener reads its own session).
    method, _ = ipc.requests[-1]
    assert method == "compute.stream.health"


async def test_open_stream_close_ends_iteration():
    ipc = _ipc({"compute.stream.open", "vision.detection.subscribe"})
    ipc.set_response(
        "compute.stream.open",
        {"execution": "offload", "opened": True, "session_id": "s1", "camera_id": "front"},
    )
    ipc.set_response("compute.stream.close", {"closed": True, "session_id": "s1"})
    compute = ComputeClient(ipc)
    session = await compute.open_stream(execution=ExecutionTier.OFFLOAD)

    consumer = asyncio.ensure_future(session.__anext__())
    for _ in range(5):
        await asyncio.sleep(0)
    # Closing unblocks a pending iterator so `async for` ends cleanly.
    await session.close()
    with pytest.raises(StopAsyncIteration):
        await asyncio.wait_for(consumer, timeout=1.0)
    # A second close is idempotent (no second RPC).
    close_calls = [m for m, _ in ipc.requests if m == "compute.stream.close"]
    await session.close()
    assert [m for m, _ in ipc.requests if m == "compute.stream.close"] == close_calls


async def test_open_stream_without_the_open_cap_is_denied():
    ipc = _ipc({"vision.detection.subscribe"})  # can consume, cannot open
    compute = ComputeClient(ipc)
    with pytest.raises(CapabilityDenied):
        await compute.open_stream(execution=ExecutionTier.OFFLOAD)
    assert ipc.requests == []


async def test_iterating_a_stream_without_the_subscribe_cap_is_denied():
    # Opening is allowed with just the open cap, but iterating (which arms the
    # detection subscription) needs vision.detection.subscribe.
    ipc = _ipc({"compute.stream.open"})
    ipc.set_response(
        "compute.stream.open",
        {"execution": "offload", "opened": True, "session_id": "s1", "camera_id": "front"},
    )
    compute = ComputeClient(ipc)
    session = await compute.open_stream(execution=ExecutionTier.OFFLOAD)
    with pytest.raises(CapabilityDenied):
        await session.__anext__()
