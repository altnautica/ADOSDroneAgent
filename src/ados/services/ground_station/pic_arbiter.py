"""Pilot-in-command arbiter.

Manages which client currently holds PIC (pilot-in-command) authority for
the ground station. Exactly one client at a time may send flight-critical
inputs to the flight controller. Other clients see a read-only view until
they request and are granted a transfer.

State machine:

    unclaimed  --claim(cid)-->            claimed(cid)
    claimed(A) --claim(B, confirm_token)-> claimed(B)
    claimed(A) --claim(B, force=True)---> claimed(B)
    claimed(A) --release(A)-------------> unclaimed
    claimed(A) --on_pic_disconnected()--> unclaimed

Events are fanned out on a `PicEventBus` that mirrors `ButtonEventBus`
from `src/ados/services/ui/events.py`.

Integration with InputManager: `run_hotplug_integration()` subscribes to
the input manager bus and auto-claims PIC on first gamepad connect when
no one holds PIC yet. A companion hook (`on_pic_disconnected`) is wired
by the REST / WebSocket layer when the PIC client drops.

Concurrency: all state mutations go through `self._lock`. Public methods
are async and safe to call from multiple tasks.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import signal
import time
from dataclasses import dataclass
from typing import AsyncIterator, Literal, Optional

import structlog

from ados.services.ground_station.input_manager import (
    InputDeviceEvent,
    get_input_manager,
)

log = structlog.get_logger(__name__)

__all__ = [
    "PicEvent",
    "PicEventBus",
    "PicArbiter",
    "get_pic_arbiter",
    "main",
]


PicEventKind = Literal["claimed", "released", "disconnected"]


@dataclass(frozen=True)
class PicEvent:
    """A single PIC state transition observation.

    kind: "claimed", "released", or "disconnected".
    client_id: identity of the client involved in the transition. For
        "released" and "disconnected" this is the client that just lost
        PIC. For "claimed" this is the new PIC holder.
    claim_counter: monotonic counter. Used by clients to detect races
        when they observe the bus out of order relative to REST replies.
    timestamp_ms: unix milliseconds at the instant of the transition.
    """

    kind: PicEventKind
    client_id: Optional[str]
    claim_counter: int
    timestamp_ms: int


class PicEventBus:
    """Asyncio fanout bus for PicEvent.

    Same pattern as `ButtonEventBus`. Each subscriber gets its own
    bounded queue. Slow subscribers drop events rather than stalling
    the publisher.
    """

    _SENTINEL: object = object()

    def __init__(self, queue_maxsize: int = 64) -> None:
        self._subscribers: list[asyncio.Queue] = []
        self._queue_maxsize = queue_maxsize
        self._closed = False
        self._lock = asyncio.Lock()

    async def publish(self, event: PicEvent) -> None:
        if self._closed:
            return
        async with self._lock:
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(event)
            except asyncio.QueueFull:
                pass

    async def subscribe(self) -> AsyncIterator[PicEvent]:
        queue: asyncio.Queue = asyncio.Queue(maxsize=self._queue_maxsize)
        async with self._lock:
            if self._closed:
                return
            self._subscribers.append(queue)
        try:
            while True:
                item = await queue.get()
                if item is self._SENTINEL:
                    return
                assert isinstance(item, PicEvent)
                yield item
        finally:
            async with self._lock:
                if queue in self._subscribers:
                    self._subscribers.remove(queue)

    async def close(self) -> None:
        async with self._lock:
            self._closed = True
            targets = list(self._subscribers)
        for q in targets:
            try:
                q.put_nowait(self._SENTINEL)
            except asyncio.QueueFull:
                try:
                    q.get_nowait()
                    q.put_nowait(self._SENTINEL)
                except Exception:
                    pass


# Confirm token lifetime. Short window forces the taking client to act
# intentionally and prevents replay long after the original warning was
# shown in the UI.
_CONFIRM_TTL_SECONDS: float = 2.0


@dataclass
class _ConfirmToken:
    client_id: str
    token: str
    expires_at: float


class PicArbiter:
    """Pilot-in-command arbiter.

    Holds the current PIC state, handles claim / release / confirm-token
    flows, and emits PIC transition events on `self.bus`.
    """

    def __init__(self) -> None:
        self.state: Literal["unclaimed", "claimed"] = "unclaimed"
        self.claimed_by: Optional[str] = None
        self.claimed_since: Optional[float] = None
        self.claim_counter: int = 0
        self.primary_gamepad_id: Optional[str] = None
        self.last_heartbeat_ts: Optional[float] = None

        self.bus = PicEventBus()
        self._lock = asyncio.Lock()
        self._confirm_tokens: dict[str, _ConfirmToken] = {}
        self._watchdog_task: Optional[asyncio.Task] = None
        self._watchdog_stop: Optional[asyncio.Event] = None

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------
    def _refresh_primary_gamepad(self) -> None:
        """Read the primary gamepad id from the input manager.

        Best-effort: if the input manager does not expose `get_primary`
        (older build or unit test stub), we leave the field as-is.
        """
        try:
            im = get_input_manager()
            getter = getattr(im, "get_primary", None)
            if callable(getter):
                primary = getter()
                if primary is None:
                    self.primary_gamepad_id = None
                elif isinstance(primary, str):
                    self.primary_gamepad_id = primary
                else:
                    self.primary_gamepad_id = getattr(primary, "device_id", None)
        except Exception as exc:
            log.debug("pic.primary_gamepad_refresh_failed", error=str(exc))

    def _now_ms(self) -> int:
        return int(time.time() * 1000)

    def _purge_expired_tokens(self) -> None:
        now = time.time()
        stale = [k for k, t in self._confirm_tokens.items() if t.expires_at < now]
        for k in stale:
            self._confirm_tokens.pop(k, None)

    def _issue_claim_locked(self, client_id: str) -> int:
        """Assume lock is held. Transition to claimed(client_id)."""
        self.state = "claimed"
        self.claimed_by = client_id
        self.claimed_since = time.time()
        self.last_heartbeat_ts = time.time()
        self.claim_counter += 1
        self._refresh_primary_gamepad()
        return self.claim_counter

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------
    async def claim(
        self,
        client_id: str,
        confirm_token: Optional[str] = None,
        force: bool = False,
    ) -> dict:
        """Attempt to claim PIC for `client_id`.

        Returns a dict describing the outcome. The REST layer maps
        `already_claimed` to HTTP 409 and surfaces `needs_confirm` so
        the GCS can show the "Take control" prompt.
        """
        async with self._lock:
            self._purge_expired_tokens()

            # Case 1: nobody holds PIC. Immediate claim.
            if self.state == "unclaimed":
                counter = self._issue_claim_locked(client_id)
                log.info(
                    "pic.claimed",
                    client_id=client_id,
                    claim_counter=counter,
                    mode="fresh",
                )
                event = PicEvent(
                    kind="claimed",
                    client_id=client_id,
                    claim_counter=counter,
                    timestamp_ms=self._now_ms(),
                )
                await self.bus.publish(event)
                return {
                    "claimed": True,
                    "claimed_by": client_id,
                    "claim_counter": counter,
                }

            # Case 2: PIC held, same client re-claims. Idempotent.
            if self.claimed_by == client_id:
                return {
                    "claimed": True,
                    "claimed_by": client_id,
                    "claim_counter": self.claim_counter,
                    "idempotent": True,
                }

            # Case 3: force takeover. Always wins, logged at WARN.
            if force:
                previous = self.claimed_by
                counter = self._issue_claim_locked(client_id)
                log.warning(
                    "pic.force_takeover",
                    previous_pic=previous,
                    new_pic=client_id,
                    claim_counter=counter,
                )
                await self.bus.publish(
                    PicEvent(
                        kind="claimed",
                        client_id=client_id,
                        claim_counter=counter,
                        timestamp_ms=self._now_ms(),
                    )
                )
                return {
                    "claimed": True,
                    "claimed_by": client_id,
                    "claim_counter": counter,
                    "forced": True,
                    "previous_pic": previous,
                }

            # Case 4: confirm-token flow.
            if confirm_token is not None:
                stored = self._confirm_tokens.get(client_id)
                if (
                    stored is not None
                    and stored.token == confirm_token
                    and stored.expires_at >= time.time()
                ):
                    previous = self.claimed_by
                    self._confirm_tokens.pop(client_id, None)
                    counter = self._issue_claim_locked(client_id)
                    log.info(
                        "pic.confirmed_takeover",
                        previous_pic=previous,
                        new_pic=client_id,
                        claim_counter=counter,
                    )
                    await self.bus.publish(
                        PicEvent(
                            kind="claimed",
                            client_id=client_id,
                            claim_counter=counter,
                            timestamp_ms=self._now_ms(),
                        )
                    )
                    return {
                        "claimed": True,
                        "claimed_by": client_id,
                        "claim_counter": counter,
                        "transferred_from": previous,
                    }
                log.info(
                    "pic.confirm_token_rejected",
                    client_id=client_id,
                    reason="missing_or_expired",
                )
                return {
                    "claimed": False,
                    "error": "invalid_confirm_token",
                    "current_pic": self.claimed_by,
                    "needs_confirm": True,
                    "status": 409,
                }

            # Case 5: already claimed, no token, no force. Return 409.
            log.info(
                "pic.claim_rejected",
                requester=client_id,
                current_pic=self.claimed_by,
            )
            return {
                "claimed": False,
                "error": "already_claimed",
                "current_pic": self.claimed_by,
                "needs_confirm": True,
                "status": 409,
            }

    async def release(self, client_id: str) -> dict:
        """Release PIC if `client_id` currently holds it."""
        async with self._lock:
            if self.state != "claimed" or self.claimed_by != client_id:
                return {
                    "released": False,
                    "error": "not_current_pic",
                    "current_pic": self.claimed_by,
                    "status": 403,
                }
            previous = self.claimed_by
            self.state = "unclaimed"
            self.claimed_by = None
            self.claimed_since = None
            counter = self.claim_counter
            log.info("pic.released", client_id=previous, claim_counter=counter)
            await self.bus.publish(
                PicEvent(
                    kind="released",
                    client_id=previous,
                    claim_counter=counter,
                    timestamp_ms=self._now_ms(),
                )
            )
            return {"released": True, "previous_pic": previous}

    async def get_state(self) -> dict:
        async with self._lock:
            self._refresh_primary_gamepad()
            return {
                "state": self.state,
                "claimed_by": self.claimed_by,
                "claimed_since": self.claimed_since,
                "claim_counter": self.claim_counter,
                "primary_gamepad_id": self.primary_gamepad_id,
            }

    async def create_confirm_token(self, client_id: str) -> str:
        """Mint a 32-char hex token for `client_id`.

        The token is bound to the requesting client. Only a subsequent
        `claim(client_id, confirm_token=...)` from the same client id
        within the TTL window completes the transfer.
        """
        token = secrets.token_hex(16)
        async with self._lock:
            self._purge_expired_tokens()
            self._confirm_tokens[client_id] = _ConfirmToken(
                client_id=client_id,
                token=token,
                expires_at=time.time() + _CONFIRM_TTL_SECONDS,
            )
            log.info(
                "pic.confirm_token_issued",
                client_id=client_id,
                ttl_seconds=_CONFIRM_TTL_SECONDS,
            )
            return token

    async def on_gamepad_connected(
        self, device_id: str, client_id_hint: str
    ) -> None:
        """Auto-claim PIC for `client_id_hint` if nobody holds it.

        Called when a gamepad hotplug event arrives from the input
        manager bus. Gives the single-operator bench rig a working
        control loop with no REST round-trip.
        """
        async with self._lock:
            if self.state == "unclaimed":
                counter = self._issue_claim_locked(client_id_hint)
                log.info(
                    "pic.auto_claim_on_gamepad",
                    device_id=device_id,
                    client_id=client_id_hint,
                    claim_counter=counter,
                )
                await self.bus.publish(
                    PicEvent(
                        kind="claimed",
                        client_id=client_id_hint,
                        claim_counter=counter,
                        timestamp_ms=self._now_ms(),
                    )
                )
            else:
                log.debug(
                    "pic.gamepad_connected_noop",
                    device_id=device_id,
                    current_pic=self.claimed_by,
                )

    async def on_pic_disconnected(self) -> None:
        """Handle PIC client disconnect (WS drop or gamepad removal).

        Clears PIC state and publishes a disconnected event. Flight
        controller failsafe is configured on the FC itself. This hook
        is the place to layer an explicit RTL trigger in a later phase.
        """
        async with self._lock:
            if self.state != "claimed":
                return
            previous = self.claimed_by
            self.state = "unclaimed"
            self.claimed_by = None
            self.claimed_since = None
            counter = self.claim_counter
            log.warning(
                "pic.disconnected",
                previous_pic=previous,
                claim_counter=counter,
                note="FC failsafe config governs RC_LOSS. RTL hook pending.",
            )
            await self.bus.publish(
                PicEvent(
                    kind="disconnected",
                    client_id=previous,
                    claim_counter=counter,
                    timestamp_ms=self._now_ms(),
                )
            )

    # ------------------------------------------------------------------
    # Session heartbeat + watchdog
    # ------------------------------------------------------------------
    # Clients holding PIC must POST /pic/heartbeat at least every
    # _HEARTBEAT_TIMEOUT_SECONDS or the server-side watchdog will
    # auto-release their claim. Prevents stale PIC state when a GCS
    # tab closes without calling /pic/release.
    _HEARTBEAT_TIMEOUT_SECONDS: float = 30.0
    _WATCHDOG_INTERVAL_SECONDS: float = 5.0

    async def heartbeat(self, client_id: str) -> dict:
        """Record a heartbeat for the active PIC holder."""
        async with self._lock:
            if self.state != "claimed" or self.claimed_by != client_id:
                return {
                    "ok": False,
                    "error": "no_active_claim",
                    "current_pic": self.claimed_by,
                    "status": 410,
                }
            self.last_heartbeat_ts = time.time()
            return {
                "ok": True,
                "claimed_by": self.claimed_by,
                "claim_counter": self.claim_counter,
                "last_heartbeat_ts": self.last_heartbeat_ts,
            }

    async def _session_watchdog(self) -> None:
        """Auto-release PIC if no heartbeat within the timeout window."""
        assert self._watchdog_stop is not None
        stop = self._watchdog_stop
        while not stop.is_set():
            try:
                await asyncio.wait_for(
                    stop.wait(), timeout=self._WATCHDOG_INTERVAL_SECONDS
                )
                return
            except asyncio.TimeoutError:
                pass

            expired_client: Optional[str] = None
            async with self._lock:
                if (
                    self.state == "claimed"
                    and self.claimed_by is not None
                    and self.last_heartbeat_ts is not None
                ):
                    age = time.time() - self.last_heartbeat_ts
                    if age > self._HEARTBEAT_TIMEOUT_SECONDS:
                        expired_client = self.claimed_by

            if expired_client is not None:
                log.info(
                    "pic.auto_release_on_timeout",
                    client_id=expired_client,
                    timeout_seconds=self._HEARTBEAT_TIMEOUT_SECONDS,
                )
                await self.release(expired_client)

    async def start_watchdog(self) -> None:
        """Start the background session watchdog task. Idempotent."""
        if self._watchdog_task is not None and not self._watchdog_task.done():
            return
        self._watchdog_stop = asyncio.Event()
        self._watchdog_task = asyncio.create_task(
            self._session_watchdog(), name="pic_session_watchdog"
        )
        log.info("pic.watchdog_started", interval=self._WATCHDOG_INTERVAL_SECONDS)

    async def stop_watchdog(self) -> None:
        """Stop the background session watchdog. Idempotent."""
        if self._watchdog_stop is not None:
            self._watchdog_stop.set()
        task = self._watchdog_task
        if task is not None and not task.done():
            try:
                await asyncio.wait_for(task, timeout=2.0)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                task.cancel()
        self._watchdog_task = None
        self._watchdog_stop = None

    # ------------------------------------------------------------------
    # Hotplug integration
    # ------------------------------------------------------------------
    async def run_hotplug_integration(
        self,
        client_id_hint: str = "hdmi-kiosk",
        stop_event: Optional[asyncio.Event] = None,
    ) -> None:
        """Subscribe to InputManager bus and auto-claim on first gamepad.

        Intended to run as a long-lived task owned by the arbiter's
        systemd unit or by the REST app's lifespan.
        """
        im = get_input_manager()
        log.info(
            "pic.hotplug_integration_start",
            client_id_hint=client_id_hint,
        )
        try:
            async for event in im.bus.subscribe():
                if stop_event is not None and stop_event.is_set():
                    break
                if not isinstance(event, InputDeviceEvent):
                    continue
                if event.kind == "connected":
                    await self.on_gamepad_connected(
                        device_id=event.device_id,
                        client_id_hint=client_id_hint,
                    )
                elif event.kind == "disconnected":
                    # If the disconnected gamepad was the primary bound
                    # to the current PIC, we treat it as a PIC drop.
                    if (
                        self.primary_gamepad_id is not None
                        and event.device_id == self.primary_gamepad_id
                    ):
                        await self.on_pic_disconnected()
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            log.error("pic.hotplug_integration_error", error=str(exc))
        finally:
            log.info("pic.hotplug_integration_stop")


# ----------------------------------------------------------------------
# Module-level singleton
# ----------------------------------------------------------------------
_instance: "PicArbiter | None" = None


def get_pic_arbiter() -> "PicArbiter":
    global _instance
    if _instance is None:
        _instance = PicArbiter()
    return _instance


# ----------------------------------------------------------------------
# Service entry point
# ----------------------------------------------------------------------
async def _run_service() -> None:
    arbiter = get_pic_arbiter()
    stop_event = asyncio.Event()

    loop = asyncio.get_running_loop()

    def _handle_signal(signame: str) -> None:
        log.info("pic.signal_received", signal=signame)
        stop_event.set()

    for signame in ("SIGINT", "SIGTERM"):
        try:
            loop.add_signal_handler(
                getattr(signal, signame),
                _handle_signal,
                signame,
            )
        except NotImplementedError:
            # Windows or restricted environments. Tests fall back to
            # stop_event being set externally.
            pass

    client_hint = os.environ.get("ADOS_PIC_CLIENT_HINT", "hdmi-kiosk")
    hotplug_task = asyncio.create_task(
        arbiter.run_hotplug_integration(
            client_id_hint=client_hint,
            stop_event=stop_event,
        )
    )

    await arbiter.start_watchdog()

    log.info(
        "pic.service_ready",
        client_hint=client_hint,
        state=arbiter.state,
    )

    await stop_event.wait()

    await arbiter.stop_watchdog()
    hotplug_task.cancel()
    try:
        await hotplug_task
    except asyncio.CancelledError:
        pass
    await arbiter.bus.close()
    log.info("pic.service_stopped")


def main() -> None:
    """Systemd entry point for `ados-pic.service`."""
    try:
        asyncio.run(_run_service())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
