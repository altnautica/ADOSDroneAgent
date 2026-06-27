"""Tests for the Python compute-offload SDK facade (``ctx.compute``).

Covers the job interface a plugin uses (register dataset, submit, read status,
read outputs, cancel), that each RPC is tagged with the capability the host
enforces, and that a missing capability is denied.
"""

from __future__ import annotations

import pytest

from ados.plugins.errors import CapabilityDenied
from ados.sdk.compute import ComputeClient, JobStatus, Submission
from ados.sdk.testing.stubs import FakeIpcClient


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
