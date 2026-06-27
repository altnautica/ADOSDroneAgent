"""``ctx.compute`` — the compute-offload facade for plugins.

A plugin with the compute capabilities submits a job to a paired compute node
(a reconstruction over a keyframe bag, or a perception / SLAM offload), uploads
its dataset, and reads the status + result. The facade tags each RPC with the
capability the host enforces; the host routes the call to the agent's compute
connection (the native Rust compute client) and returns the node's reply. The
plugin never gets raw compute access — only the job interface.

Capabilities:
- ``compute.dataset.write`` — register a dataset the job consumes.
- ``compute.job.submit`` — submit (and cancel) a job.
- ``compute.job.read`` — read a job's status + outputs.

The facade gates nothing itself; the host enforces the capability. The job kind
is one of ``reconstruct`` | ``perception_offload`` | ``slam_offload`` (the
node's job-kind wire strings).
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from ados.plugins.ipc_client import PluginIpcClient

# Capabilities the host enforces on each call.
CAP_DATASET_WRITE = "compute.dataset.write"
CAP_JOB_SUBMIT = "compute.job.submit"
CAP_JOB_READ = "compute.job.read"

# RPC method names the host routes to the compute connection.
_M_DATASET = "compute.dataset.write"
_M_SUBMIT = "compute.job.submit"
_M_STATUS = "compute.job.read"
_M_OUTPUTS = "compute.job.outputs"
_M_CANCEL = "compute.job.cancel"

#: The job kinds a plugin can submit (the node's wire strings).
JOB_KINDS = ("reconstruct", "perception_offload", "slam_offload")


@dataclass(frozen=True)
class Submission:
    """The reply to a job submission."""

    job_id: str
    state: str


@dataclass(frozen=True)
class JobStatus:
    """A job's status + progress."""

    job_id: str
    state: str
    progress: float
    result_ref: str | None
    error: str | None


class ComputeClient:
    """``ctx.compute`` — the compute-offload job interface for a plugin."""

    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def _request(
        self, method: str, capability: str, args: dict[str, Any]
    ) -> dict[str, Any]:
        """Send one compute RPC through the IPC client's generic sender.

        The IPC client owns the wire (request id, token, capability tag) and the
        host enforces the capability; this only tags it on the envelope.
        """
        send = getattr(self._ipc, "_send_request", None)
        if send is None:
            raise RuntimeError("ipc client exposes no request sender")
        env = await send(method, capability=capability, args=args)
        result = getattr(env, "args", {})
        return result if isinstance(result, dict) else {}

    async def write_dataset(
        self, kind: str, meta: dict[str, Any] | None = None
    ) -> str:
        """Register a dataset (``compute.dataset.write``). Returns its id."""
        resp = await self._request(
            _M_DATASET, CAP_DATASET_WRITE, {"kind": kind, "meta": meta or {}}
        )
        dataset_id = resp.get("id")
        if not dataset_id:
            raise RuntimeError("compute node returned no dataset id")
        return str(dataset_id)

    async def submit_job(
        self,
        kind: str,
        *,
        dataset_id: str | None = None,
        params: dict[str, Any] | None = None,
    ) -> Submission:
        """Submit a job (``compute.job.submit``).

        ``kind`` is one of :data:`JOB_KINDS`. A reconstruction takes a
        ``dataset_id``; a perception / SLAM offload carries its frame in
        ``params``.
        """
        if kind not in JOB_KINDS:
            raise ValueError(f"unknown job kind {kind!r}; expected one of {JOB_KINDS}")
        resp = await self._request(
            _M_SUBMIT,
            CAP_JOB_SUBMIT,
            {"kind": kind, "dataset_id": dataset_id, "params": params or {}},
        )
        return Submission(
            job_id=str(resp.get("job_id", "")),
            state=str(resp.get("state", "")),
        )

    async def read_job(self, job_id: str) -> JobStatus:
        """Read a job's status + progress (``compute.job.read``)."""
        resp = await self._request(_M_STATUS, CAP_JOB_READ, {"job_id": job_id})
        progress = resp.get("progress", 0.0)
        # The node's job record names its id `id`; the submit reply names it
        # `job_id`. Read either, falling back to the id we asked about.
        return JobStatus(
            job_id=str(resp.get("id") or resp.get("job_id") or job_id),
            state=str(resp.get("state", "")),
            progress=float(progress) if isinstance(progress, (int, float)) else 0.0,
            result_ref=resp.get("result_ref"),
            error=resp.get("error"),
        )

    async def job_outputs(self, job_id: str) -> list[dict[str, Any]]:
        """Read a finished job's outputs (``compute.job.read``)."""
        resp = await self._request(_M_OUTPUTS, CAP_JOB_READ, {"job_id": job_id})
        outputs = resp.get("outputs", [])
        if not isinstance(outputs, list):
            return []
        return [o for o in outputs if isinstance(o, dict)]

    async def cancel_job(self, job_id: str) -> bool:
        """Cancel a job (``compute.job.submit``). Returns whether it was cancelled."""
        resp = await self._request(_M_CANCEL, CAP_JOB_SUBMIT, {"job_id": job_id})
        return bool(resp.get("cancelled", False))
