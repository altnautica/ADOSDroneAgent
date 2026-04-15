"""StateReader. Maintains a live VehicleState by reading the ados-mavlink
state IPC socket.

The cloud relay bridge owns one of these. It hands the embedded
VehicleState to MqttGateway and to the Convex heartbeat path so both
publish fresh telemetry instead of a stub.

Pattern mirrors the existing StateIPC consumers:
- `services/cloud/__main__.py:204-227` (state_handler + read_loop with
  reconnect)
- `services/scripting/__main__.py:85-92` (poll-based state copy)

We use the cloud-service style (event-driven `set_state_handler` plus a
self-healing reconnect loop) because the bridge already runs an asyncio
task supervisor and benefits from push updates rather than 10Hz polling.

Phase 4 Wave 2 fix for Wave B blocker 1.
"""

from __future__ import annotations

import asyncio
from typing import Optional

import structlog

from ados.core.ipc import StateIPCClient
from ados.services.mavlink.state import VehicleState

log = structlog.get_logger("ground_station.state_reader")


_CONNECT_RETRIES = 3
_CONNECT_DELAY_SECONDS = 2.0
_RECONNECT_BACKOFF_SECONDS = 2.0


class StateReader:
    """Live VehicleState backed by the ados-mavlink state IPC socket.

    Owns a single VehicleState instance that mirrors what
    `ados-mavlink` publishes on `/run/ados/state.sock` (JSON line
    snapshots, see `core.ipc.StateIPCServer`). On socket failure the
    reader logs once, falls back to the stub VehicleState, and
    auto-reconnects in the background.
    """

    def __init__(
        self,
        vehicle_state: Optional[VehicleState] = None,
        sock_client: Optional[StateIPCClient] = None,
    ) -> None:
        self.vehicle_state: VehicleState = vehicle_state or VehicleState()
        self._client: StateIPCClient = sock_client or StateIPCClient()
        self._task: Optional[asyncio.Task] = None
        self._stop_event: Optional[asyncio.Event] = None
        self._connected_once: bool = False
        self._fallback_warned: bool = False

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------
    async def start(self) -> None:
        """Begin background read loop. Non-blocking. Falls back to stub
        VehicleState on connect failure without raising.
        """
        if self._task is not None and not self._task.done():
            return

        self._client.set_state_handler(self._on_state_update)
        self._stop_event = asyncio.Event()

        try:
            await self._client.connect(
                retries=_CONNECT_RETRIES, delay=_CONNECT_DELAY_SECONDS
            )
            self._connected_once = True
            log.info("state_reader.connected")
        except ConnectionError as exc:
            if not self._fallback_warned:
                log.warning(
                    "state_reader.fallback_stub_state",
                    reason=str(exc),
                    note=(
                        "ados-mavlink state IPC unavailable. "
                        "Publishing stub VehicleState. "
                        "Will keep retrying in background."
                    ),
                )
                self._fallback_warned = True

        self._task = asyncio.create_task(
            self._run_with_reconnect(), name="state-reader"
        )

    async def stop(self) -> None:
        if self._stop_event is not None:
            self._stop_event.set()
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except (asyncio.CancelledError, Exception):
                pass
            self._task = None
        try:
            await self._client.disconnect()
        except Exception:
            pass

    # ------------------------------------------------------------------
    # Snapshot accessor
    # ------------------------------------------------------------------
    def get_latest(self) -> VehicleState:
        """Return the live VehicleState. Same object every call so
        downstream consumers (MqttGateway) can hold a long-lived
        reference.
        """
        return self.vehicle_state

    @property
    def connected(self) -> bool:
        return bool(self._client.connected)

    # ------------------------------------------------------------------
    # Internal
    # ------------------------------------------------------------------
    def _on_state_update(self, state_dict: dict) -> None:
        if not state_dict:
            return
        try:
            self.vehicle_state.update_from_dict(state_dict)
        except Exception as exc:
            log.debug("state_reader.update_failed", error=str(exc))

    async def _run_with_reconnect(self) -> None:
        """Read JSON state lines from the socket. Reconnect on drop."""
        assert self._stop_event is not None
        while not self._stop_event.is_set():
            try:
                if not self._client.connected:
                    await self._client.connect(
                        retries=_CONNECT_RETRIES,
                        delay=_CONNECT_DELAY_SECONDS,
                    )
                    if not self._connected_once:
                        log.info("state_reader.connected_late")
                    self._connected_once = True
                    self._fallback_warned = False
                await self._client.read_loop()
            except ConnectionError as exc:
                log.debug("state_reader.connect_failed", error=str(exc))
            except asyncio.CancelledError:
                break
            except Exception as exc:
                log.warning("state_reader.read_loop_failed", error=str(exc))

            if self._stop_event.is_set():
                break
            try:
                await asyncio.wait_for(
                    self._stop_event.wait(),
                    timeout=_RECONNECT_BACKOFF_SECONDS,
                )
                break
            except asyncio.TimeoutError:
                pass
