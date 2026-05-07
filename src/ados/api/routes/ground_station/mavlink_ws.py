"""MAVLink WebSocket bridge for ground-station clients.

Bridges raw MAVLink v2 frames between the local MAVLink IPC socket
(``/run/ados/mavlink.sock``) and a WebSocket consumer. Native ground-
station GCS clients open this endpoint to receive telemetry from the
paired drone and to inject commands without speaking the internal
length-prefixed IPC framing or MQTT topics.

Wire format on the WebSocket: each binary frame is one or more raw
MAVLink v2 packets (magic ``0xFD``). No JSON wrapping, no length
prefix, no newline delimiting. The IPC layer's length-prefix framing
is unwrapped here and the inner MAVLink bytes are forwarded verbatim.

Uplink (client to FC) follows the same shape: each incoming binary
WebSocket frame is fed into ``MavlinkIPCClient.send()``, which adds
the length prefix expected by the IPC server.

Profile-gated to ``ground_station`` like all other ground-station
endpoints. Drone-profile callers close with code 1008.
"""

from __future__ import annotations

import asyncio

import structlog
from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from ados.api.deps import get_agent_app
from ados.core.ipc import MAVLINK_SOCK, MavlinkIPCClient

log = structlog.get_logger("api.ground_station.mavlink_ws")

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])


# Bound the per-connection downlink queue so a slow WebSocket peer
# cannot grow memory without limit when the upstream IPC keeps producing
# frames. At ~50 Hz aggregate from the FC, 256 frames is roughly five
# seconds of buffering, matching the IPC layer's own headroom.
_WS_DOWNLINK_QUEUE_DEPTH = 256


@router.websocket("/ws/mavlink")
async def ws_mavlink_bridge(websocket: WebSocket) -> None:
    """Stream raw MAVLink v2 frames between the WebSocket and the IPC bus.

    Two parallel tasks run for the lifetime of the connection:

    * **Downlink** copies frames from the MAVLink IPC client (FC -> GCS)
      into the WebSocket as binary frames.
    * **Uplink** reads binary frames from the WebSocket (GCS -> FC) and
      forwards them to the IPC client, which delivers them back to the
      MAVLink service for injection on the FC link.

    Either task ending (peer disconnect, IPC error) cancels the other
    and tears the connection down.
    """
    app = get_agent_app()
    profile = getattr(app.config.agent, "profile", "auto")
    if profile != "ground_station":
        await websocket.close(code=1008, reason="E_PROFILE_MISMATCH")
        return

    await websocket.accept()

    ipc = MavlinkIPCClient(sock_path=MAVLINK_SOCK)
    try:
        await ipc.connect(retries=3, delay=0.5)
    except ConnectionError as exc:
        log.warning("mavlink_ws_ipc_unavailable", error=str(exc))
        try:
            await websocket.close(code=1011, reason="E_MAVLINK_IPC_UNAVAILABLE")
        except RuntimeError:
            pass
        return

    # Downlink frames buffered through a bounded queue. The IPC client
    # invokes its data handler from its own read_loop, which means the
    # callback is synchronous from this task's perspective. The queue
    # decouples that producer from the WebSocket writer so a slow peer
    # cannot stall the IPC reader.
    downlink: asyncio.Queue[bytes] = asyncio.Queue(maxsize=_WS_DOWNLINK_QUEUE_DEPTH)

    def _on_ipc_frame(data: bytes) -> None:
        try:
            downlink.put_nowait(data)
        except asyncio.QueueFull:
            # Drop oldest to keep recency. Telemetry is more useful fresh
            # than complete; the same policy the cloud relay uses.
            try:
                _ = downlink.get_nowait()
                downlink.put_nowait(data)
            except (asyncio.QueueEmpty, asyncio.QueueFull):
                pass

    ipc.set_data_handler(_on_ipc_frame)

    async def _ipc_read() -> None:
        """Drive the IPC client read loop. Runs until IPC disconnects."""
        try:
            await ipc.read_loop()
        except Exception as exc:  # pragma: no cover - defensive
            log.debug("mavlink_ws_ipc_read_exit", error=str(exc))

    async def _downlink_pump() -> None:
        """Drain the downlink queue into the WebSocket."""
        try:
            while True:
                frame = await downlink.get()
                await websocket.send_bytes(frame)
        except WebSocketDisconnect:
            return
        except (RuntimeError, ConnectionResetError):
            # WebSocket closed under us. The peer-side read task will
            # also unblock and tear down.
            return

    async def _uplink_pump() -> None:
        """Forward WebSocket binary frames into the IPC uplink path."""
        try:
            while True:
                data = await websocket.receive_bytes()
                if not data:
                    continue
                try:
                    ipc.send(data)
                except Exception as exc:
                    # A malformed frame should not kill the session.
                    # Log once and keep reading; the next valid frame
                    # will go through.
                    log.warning("mavlink_ws_uplink_send_failed", error=str(exc))
        except WebSocketDisconnect:
            return
        except (RuntimeError, ConnectionResetError):
            return

    ipc_task = asyncio.create_task(_ipc_read(), name="mavlink-ws-ipc-read")
    down_task = asyncio.create_task(_downlink_pump(), name="mavlink-ws-downlink")
    up_task = asyncio.create_task(_uplink_pump(), name="mavlink-ws-uplink")

    try:
        done, pending = await asyncio.wait(
            [ipc_task, down_task, up_task],
            return_when=asyncio.FIRST_COMPLETED,
        )
        for task in pending:
            task.cancel()
        await asyncio.gather(*pending, return_exceptions=True)
    except WebSocketDisconnect:
        pass
    except Exception as exc:
        log.warning("mavlink_ws_bridge_error", error=str(exc))
    finally:
        for task in (ipc_task, down_task, up_task):
            if not task.done():
                task.cancel()
        await asyncio.gather(ipc_task, down_task, up_task, return_exceptions=True)
        try:
            await ipc.disconnect()
        except Exception:
            pass
        # Best-effort close. If the peer already hung up, FastAPI will
        # raise RuntimeError on a second close; swallow.
        try:
            await websocket.close()
        except RuntimeError:
            pass
