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
- ``compute.stream.open`` — open (and close / read-health of) a streaming
  perception-offload session (:meth:`ComputeClient.open_stream`); iterating the
  returned batches also needs ``vision.detection.subscribe``.

The facade gates nothing itself; the host enforces the capability. The job kind
is one of ``reconstruct`` | ``perception_offload`` | ``slam_offload`` (the
node's job-kind wire strings). A streaming session runs a plugin's model on the
drone's local accelerator or on the paired compute node
(:class:`~ados.sdk.offload.ExecutionTier`), transparently on the shared
detection bus.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from ados.sdk.offload import ExecutionTier, ResolvedTier

if TYPE_CHECKING:
    from ados.plugins.ipc_client import PluginIpcClient

# Capabilities the host enforces on each call.
CAP_DATASET_WRITE = "compute.dataset.write"
CAP_JOB_SUBMIT = "compute.job.submit"
CAP_JOB_READ = "compute.job.read"
CAP_STREAM_OPEN = "compute.stream.open"
# Consuming the returned detections rides the shared vision detection bus, so it
# is gated on the vision subscribe cap (the same one a local detector consumer
# uses); a plugin that only opens/controls a stream never needs it.
CAP_DETECTION_SUBSCRIBE = "vision.detection.subscribe"

# RPC method names the host routes to the compute connection.
_M_DATASET = "compute.dataset.write"
_M_SUBMIT = "compute.job.submit"
_M_STATUS = "compute.job.read"
_M_OUTPUTS = "compute.job.outputs"
_M_CANCEL = "compute.job.cancel"
_M_STREAM_OPEN = "compute.stream.open"
_M_STREAM_CLOSE = "compute.stream.close"
_M_STREAM_HEALTH = "compute.stream.health"

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


# Sentinel pushed onto a session's queue to unblock a pending iterator when the
# stream is closed, so `async for` ends cleanly (never a fabricated batch).
_STREAM_CLOSED = object()


class OffloadStreamSession:
    """A live streaming perception-offload session a plugin opened.

    Returned by :meth:`ComputeClient.open_stream`. The plugin iterates the
    returned detection batches (``async for batch in session``), reads the
    session's live health from the node's registry (``await session.health()``),
    and closes it (``await session.close()``).

    The batches are the same :class:`~ados.sdk.vision.DetectionBatch` the vision
    engine publishes — whether inference ran on the drone (local tier) or on the
    paired compute node (offload tier), the detections land on the shared
    ``vision.detection`` bus, so iterating a session is execution-transparent.
    The detection subscription is lazy: it is armed on the first iteration, so a
    plugin that only opens and closes a session (never iterates) does not need
    the ``vision.detection.subscribe`` capability.

    :attr:`execution` is the :class:`~ados.sdk.offload.ResolvedTier` the host
    resolved for this session (the agent's ``pick_tier`` decision — one of
    ``local`` / ``offload`` / ``hybrid`` / ``none``, NOT the plugin's
    :class:`~ados.sdk.offload.ExecutionTier` intent), and :attr:`opened` is
    whether an offload session was actually started (``False`` when the runtime
    resolved to local — the plugin then runs its model on the local accelerator
    via ``ctx.vision``).
    """

    def __init__(
        self,
        *,
        ipc: PluginIpcClient,
        session_id: str,
        camera_id: str,
        execution: ResolvedTier,
        opened: bool,
        source: str | None = None,
        node: str | None = None,
    ) -> None:
        self._ipc = ipc
        self.session_id = session_id
        self.camera_id = camera_id
        self.execution = execution
        self.opened = opened
        self.source = source
        self.node = node
        self._queue: asyncio.Queue[Any] = asyncio.Queue()
        self._subscribed = False
        self._closed = False

    async def _request(
        self, method: str, capability: str, args: dict[str, Any]
    ) -> dict[str, Any]:
        send = getattr(self._ipc, "_send_request", None)
        if send is None:
            raise RuntimeError("ipc client exposes no request sender")
        env = await send(method, capability=capability, args=args)
        result = getattr(env, "args", {})
        return result if isinstance(result, dict) else {}

    async def _subscribe(self) -> None:
        """Arm the detection subscription (once) so returned batches flow into the
        iterator queue. Gated on ``vision.detection.subscribe`` — the same bus a
        local detector consumer reads, so an offloaded batch is indistinguishable
        from a local one."""
        # Imported here (not at module load) so opening/closing a stream does not
        # pull the vision graph when a plugin never iterates.
        import msgpack

        from ados.sdk.vision import SUBSCRIBE_DETECTIONS, DetectionBatch

        self._subscribed = True
        want = self.camera_id

        async def _on_batch(payload: dict[str, Any]) -> None:
            raw = payload.get("batch")
            if not isinstance(raw, (bytes, bytearray, memoryview)):
                return
            try:
                batch = DetectionBatch.from_msgpack(bytes(raw))
            except (ValueError, KeyError, msgpack.UnpackException):
                return
            if want and batch.camera_id != want:
                return
            await self._queue.put(batch)

        sub_args: dict[str, Any] = {"camera_id": want} if want else {}
        await self._request(
            SUBSCRIBE_DETECTIONS, CAP_DETECTION_SUBSCRIBE, sub_args
        )
        await self._ipc.vision_subscribe_detections(_on_batch)

    def __aiter__(self) -> OffloadStreamSession:
        return self

    async def __anext__(self) -> Any:
        if self._closed:
            raise StopAsyncIteration
        if not self._subscribed:
            await self._subscribe()
        batch = await self._queue.get()
        if batch is _STREAM_CLOSED:
            raise StopAsyncIteration
        return batch

    async def health(self) -> dict[str, Any]:
        """Read the session's live health from the node's session registry
        (state, throughput, reconnect / restart history). A session the node has
        reaped reads as ``{"state": "closed", "found": False}``. Gated on
        ``compute.stream.open`` (the opener reads its own session)."""
        return await self._request(
            _M_STREAM_HEALTH, CAP_STREAM_OPEN, {"session_id": self.session_id}
        )

    async def close(self) -> None:
        """Close the session: the host cancels the offload lane and settles the
        safety gate to Lost. Idempotent. Gated on ``compute.stream.open``."""
        if self._closed:
            return
        self._closed = True
        try:
            await self._request(
                _M_STREAM_CLOSE, CAP_STREAM_OPEN, {"session_id": self.session_id}
            )
        finally:
            # Unblock a pending iterator so `async for` ends cleanly.
            await self._queue.put(_STREAM_CLOSED)

    async def __aenter__(self) -> OffloadStreamSession:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()


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

    async def open_stream(
        self,
        *,
        camera_id: str = "front",
        execution: ExecutionTier | str = ExecutionTier.AUTO,
        session_id: str | None = None,
        width: int | None = None,
        height: int | None = None,
        target_budget_ms: int | None = None,
        model_id: str | None = None,
    ) -> OffloadStreamSession:
        """Open a streaming perception-offload session (``compute.stream.open``).

        The plugin's detection model runs on the drone's local accelerator
        (:attr:`ExecutionTier.LOCAL`), on a paired compute node
        (:attr:`ExecutionTier.OFFLOAD`), or wherever the runtime picks
        (:attr:`ExecutionTier.AUTO`) — either way the detections return on the
        shared ``vision.detection`` bus. The host owns the source (the drone's
        LAN-reachable camera feed) and the node reach; the plugin names only its
        camera, model, and intent.

        The host resolves the perception tier (``ados_offload::pick_tier`` reading
        the offload-link sidecar — the tier logic is not duplicated here) and
        reports it on the returned session's :attr:`execution` as a
        :class:`~ados.sdk.offload.ResolvedTier` (``local`` / ``offload`` /
        ``hybrid`` / ``none`` — a superset of the ``ExecutionTier`` intent). For
        ``AUTO``/``OFFLOAD`` on an NPU-less drone with a paired node it starts the
        node-side session (``session.opened`` is ``True``); when it resolves to
        local it starts none (``opened`` is ``False``) and the plugin runs its
        model locally via ``ctx.vision``.

        Returns an :class:`OffloadStreamSession`: iterate it for the returned
        detection batches (needs ``vision.detection.subscribe``), read
        ``session.health()``, and ``session.close()`` it.
        """
        args: dict[str, Any] = {
            "execution": ExecutionTier(execution).value,
            "camera_id": camera_id,
        }
        if session_id is not None:
            args["session_id"] = session_id
        if width is not None:
            args["width"] = int(width)
        if height is not None:
            args["height"] = int(height)
        if target_budget_ms is not None:
            args["target_budget_ms"] = int(target_budget_ms)
        if model_id is not None:
            args["model_id"] = model_id

        resp = await self._request(_M_STREAM_OPEN, CAP_STREAM_OPEN, args)
        return OffloadStreamSession(
            ipc=self._ipc,
            session_id=str(resp.get("session_id", session_id or "")),
            camera_id=str(resp.get("camera_id", camera_id)),
            # The host reports the RESOLVED tier (local/offload/hybrid/none), a
            # superset of the ExecutionTier intent — parse it into the total
            # ResolvedTier so a hybrid/none/unknown reply never raises here.
            execution=ResolvedTier.parse(str(resp.get("execution", "local"))),
            opened=bool(resp.get("opened", False)),
            source=resp.get("source"),
            node=resp.get("node"),
        )
