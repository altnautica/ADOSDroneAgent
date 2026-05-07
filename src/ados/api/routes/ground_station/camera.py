"""Camera-source switch endpoint for the ground-station profile.

Lets a paired multi-camera drone toggle between its onboard camera
sources from a connected GCS. Sends a single ``MAV_CMD_SET_CAMERA_SOURCE``
COMMAND_LONG packet over the local MAVLink IPC bus, which the agent's
MAVLink service relays to the FC over the WFB-ng radio link.

Routes:

* POST /camera/switch  {camera_id: str}

Single-camera or unspecified drones return HTTP 501 so the GCS can
surface the "not supported by this drone" hint without trying to parse
a 400. Multi-camera drones return 200 with the COMMAND_LONG accepted
(fire-and-forget; the FC's COMMAND_ACK lands on the MAVLink WS bridge
and is consumed by the GCS, not by this endpoint).
"""

from __future__ import annotations

import asyncio
import re

import structlog
from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, Field
from pymavlink.dialects.v20 import common as mavlink2

from ados.api.routes import ground_station as _gs
from ados.core.ipc import MAVLINK_SOCK, MavlinkIPCClient

log = structlog.get_logger("api.ground_station.camera")

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


# Target system / component used when injecting commands toward the FC.
# Matches the values the scripting translator uses for COMMAND_LONG.
_TARGET_SYS = 1
_TARGET_COMP = 1

# Ground source IDs are the ADOS-side numbering callers send. Internally
# the FC uses 1-based camera-source indices on MAV_CMD_SET_CAMERA_SOURCE.
# The accepted form is a small positive integer encoded as a string so
# the wire contract stays symmetrical with future named-source variants.
_CAMERA_ID_RE = re.compile(r"^[A-Za-z0-9_\-]{1,32}$")


class CameraSwitchRequest(BaseModel):
    """POST body for selecting a camera source on the paired drone."""

    camera_id: str = Field(min_length=1, max_length=32)


class CameraSwitchResponse(BaseModel):
    """Result of a camera switch request."""

    camera_id: str
    accepted: bool
    reason: str | None = None


def _paired_drone_camera_count() -> int:
    """Return the number of cameras advertised by the paired drone.

    Forward-looking placeholder. The drone-side agent has not yet wired
    a camera-count capability into its heartbeat, so we default to 1
    (single-camera) and let tests monkeypatch this helper to simulate a
    multi-camera drone. When the drone agent starts publishing
    ``camera_count`` through the MQTT or IPC capability surface, swap
    the body of this function to read from that source. The
    501-vs-200 decision in ``post_camera_switch`` keys on the return
    value, so changing the source is non-breaking for existing tests.
    """
    return 1


def _resolve_camera_index(camera_id: str) -> int | None:
    """Map a ground-side camera_id string to a MAVLink source index.

    Today only numeric ids ("1", "2", ...) are supported; a future
    named-source mapping (``thermal``, ``rgb``, ``zoom``) plugs in here
    without changing the route signature. Returns None when the id
    cannot be resolved to a positive integer index.
    """
    if not _CAMERA_ID_RE.match(camera_id):
        return None
    try:
        idx = int(camera_id)
    except ValueError:
        return None
    if idx < 1:
        return None
    return idx


def _build_set_camera_source_bytes(camera_index: int) -> bytes:
    """Encode MAV_CMD_SET_CAMERA_SOURCE as a COMMAND_LONG wire frame.

    Built fresh per call so the encoder's monotonic sequence number is
    not shared with other callers. Component id in param1 stays 0
    (broadcast to camera) per the MAVLink common spec; the source id
    rides in param2 as a 1-based index.
    """
    encoder = mavlink2.MAVLink(None, srcSystem=255, srcComponent=190)
    encoder.robust_parsing = True
    msg = encoder.command_long_encode(
        _TARGET_SYS,
        _TARGET_COMP,
        mavlink2.MAV_CMD_SET_CAMERA_SOURCE,
        0,  # confirmation
        0.0,  # param1: camera component id (0 = all)
        float(camera_index),  # param2: primary source id
        0.0,  # param3: secondary source id (unused)
        0.0,
        0.0,
        0.0,
        0.0,
    )
    return msg.pack(encoder)


async def _send_via_mavlink_ipc(payload: bytes) -> None:
    """Open a short-lived IPC client, push the frame, disconnect."""
    ipc = MavlinkIPCClient(sock_path=MAVLINK_SOCK)
    try:
        await ipc.connect(retries=2, delay=0.25)
    except ConnectionError as exc:
        raise HTTPException(
            status_code=503,
            detail={
                "error": {
                    "code": "E_MAVLINK_IPC_UNAVAILABLE",
                    "message": str(exc),
                }
            },
        ) from exc

    try:
        ipc.send(payload)
        # Yield once so the kernel buffer ships before disconnect.
        await asyncio.sleep(0)
    finally:
        try:
            await ipc.disconnect()
        except Exception:
            pass


@router.post("/camera/switch", response_model=CameraSwitchResponse)
async def post_camera_switch(req: CameraSwitchRequest) -> CameraSwitchResponse:
    """Switch the paired drone's active camera source.

    Returns 501 when the paired drone does not advertise multi-camera
    support; the GCS surfaces this as "not supported by this drone".
    Returns 400 when the camera_id is malformed (non-numeric or
    out-of-range). Returns 503 when the local MAVLink IPC bus cannot
    be reached. Returns 200 + accepted=True otherwise.
    """
    _gs._require_ground_profile()

    accessor = getattr(_gs, "_paired_drone_camera_count", None)
    if callable(accessor):
        try:
            count = int(accessor())
        except Exception:
            count = 1
    else:
        count = _paired_drone_camera_count()

    if count <= 1:
        log.info(
            "camera_switch_unsupported",
            camera_count=count,
            requested=req.camera_id,
        )
        raise HTTPException(
            status_code=501,
            detail="drone does not advertise multi-camera support",
        )

    index = _resolve_camera_index(req.camera_id)
    if index is None or index > count:
        log.warning(
            "camera_switch_invalid_id",
            requested=req.camera_id,
            camera_count=count,
        )
        raise HTTPException(
            status_code=400,
            detail={
                "error": {
                    "code": "E_INVALID_CAMERA_ID",
                    "message": "camera_id must be a positive integer within the advertised range",
                }
            },
        )

    payload = _build_set_camera_source_bytes(index)
    await _send_via_mavlink_ipc(payload)

    log.info(
        "camera_switch_dispatched",
        camera_id=req.camera_id,
        camera_index=index,
        camera_count=count,
    )
    return CameraSwitchResponse(
        camera_id=req.camera_id,
        accepted=True,
        reason=None,
    )
