"""Accept-window state, UDP join listener and socket/timer plumbing for
`pairing_manager.PairingManager`."""

from __future__ import annotations

import asyncio
import json
import socket
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, cast

from ados.core.logging import get_logger

from .events import PairingEvent, PairingEventBus
from .pair_journal import publish_pair_event as _publish_pair_event

if TYPE_CHECKING:
    from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey

    from .pairing_manager import PairingManager

log = get_logger("ground_station.pairing_manager")

PAIR_UDP_PORT = 5801
MESH_IFACE = "bat0"


def _resolve_bat0_ip_or_fallback() -> str:
    """Return the IPv4 address of `bat0`. If the mesh interface does
    not exist yet, or it has no IPv4 assignment, fall back to
    `0.0.0.0` and log the wider scope."""
    try:
        import fcntl
        import struct as _struct

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            packed = _struct.pack("256s", MESH_IFACE.encode("ascii"))
            # SIOCGIFADDR = 0x8915
            ip_bytes = fcntl.ioctl(sock.fileno(), 0x8915, packed)[20:24]
        finally:
            sock.close()
        return socket.inet_ntoa(ip_bytes)
    except OSError:
        log.warning(
            "pairing_bat0_ip_unavailable_falling_back",
            detail="Binding UDP 5801 to 0.0.0.0. The mesh carrier does "
                   "not have an IPv4 address yet.",
        )
        return "0.0.0.0"


@dataclass
class PendingRequest:
    """A relay's join request waiting for operator approval."""

    device_id: str
    relay_pubkey: bytes  # 32-byte X25519 public key
    remote_addr: tuple[str, int]
    received_at_ms: int


@dataclass
class AcceptWindow:
    # Wall-clock timestamps kept for UI display and REST snapshots.
    # Do NOT use them for freshness checks; wall-clock can go backwards
    # on NTP step corrections and expire a valid window early or accept
    # a stale one. `closes_at_monotonic_ns` is the authoritative deadline.
    opened_at_ms: int
    closes_at_ms: int
    # The code the receiver shows and the relay's operator types in. It
    # is bound into every invite's key for this window.
    code: str
    # Authoritative deadline for `is_window_open` and `_expire_at`.
    # Populated when the window is created.
    closes_at_monotonic_ns: int = 0
    pending: list[PendingRequest] = field(default_factory=list)
    approvals: dict[str, int] = field(default_factory=dict)  # device_id -> ts



class _PairingProtocol(asyncio.DatagramProtocol):
    """UDP datagram handler for incoming relay join requests.

    Wire format (JSON one line per datagram):
        {"type": "join", "device_id": "<id>", "pubkey_hex": "<64-hex>"}

    Reply is the encrypted invite blob from `encrypt_invite`, sent by
    the receiver when `approve()` is called. The relay listens on the
    same socket for the reply.
    """

    def __init__(self, manager: PairingManager) -> None:
        self._manager = manager
        self.transport: asyncio.DatagramTransport | None = None

    def connection_made(self, transport: asyncio.BaseTransport) -> None:  # noqa: D401
        self.transport = transport  # type: ignore[assignment]

    def datagram_received(self, data: bytes, addr: tuple) -> None:
        try:
            msg = json.loads(data.decode("utf-8"))
        except (UnicodeDecodeError, ValueError):
            log.debug("pairing_recv_bad_payload", addr=addr)
            return
        if msg.get("type") != "join":
            return
        device_id = msg.get("device_id")
        pubkey_hex = msg.get("pubkey_hex")
        if not device_id or not pubkey_hex:
            return
        try:
            pubkey = bytes.fromhex(pubkey_hex)
        except ValueError:
            return
        if len(pubkey) != 32:
            return
        # Hand off to the manager. The coroutine writes the state under
        # its lock and publishes the PairingEvent.
        asyncio.create_task(
            self._manager.submit_request(device_id, pubkey, addr)
        )

    def error_received(self, exc: Exception) -> None:
        log.debug("pairing_udp_error", error=str(exc))



class _PairingSocketMixin:
    """UDP listener and window-timer methods of `PairingManager`."""

    _window: AcceptWindow | None
    _priv: X25519PrivateKey | None
    _bus: PairingEventBus
    _lock: asyncio.Lock
    _transport: asyncio.DatagramTransport | None
    _protocol: _PairingProtocol | None
    _expire_task: asyncio.Task[None] | None
    _tick_task: asyncio.Task[None] | None

    async def _bind_socket(self, bind_addr: str | None = None) -> bool:
        """Bring up the UDP listener. Idempotent.

        Binds to the mesh interface (`bat0` by default) when it has an
        IPv4 address. This stops off-mesh attackers on a shared LAN from
        reaching UDP 5801. If the mesh carrier is not up yet or does not
        have an IP, falls back to `0.0.0.0` and logs a warning so the
        operator sees why the bind is wider than it should be.

        Pre-creates the socket with SO_REUSEADDR so a fast restart
        (agent crash-and-restart within the kernel's rebind wait window)
        can reclaim UDP 5801 without the bind failing.
        """
        if self._transport is not None:
            return True
        if bind_addr is None:
            bind_addr = _resolve_bat0_ip_or_fallback()
        loop = asyncio.get_running_loop()
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            # SO_REUSEPORT is Linux-specific and best-effort. It lets two
            # processes bind the same UDP port concurrently during a
            # rolling restart; the old process drains while the new one
            # accepts. Not fatal if the kernel rejects it.
            try:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
            except (AttributeError, OSError):
                pass
            # SO_BINDTODEVICE restricts the socket to packets that
            # ingressed on the named interface. Requires CAP_NET_RAW
            # which the service unit declares. Best-effort: on some
            # kernels this errors for unprivileged callers, on non-Linux
            # (macOS dev) the constant may not exist. Falling back to
            # the bind-to-IP above still scopes reachability.
            if bind_addr != "0.0.0.0":
                try:
                    sock.setsockopt(
                        socket.SOL_SOCKET,
                        getattr(socket, "SO_BINDTODEVICE", 25),
                        b"bat0\0",
                    )
                except (OSError, PermissionError):
                    pass
            sock.setblocking(False)
            sock.bind((bind_addr, PAIR_UDP_PORT))
            transport, protocol = await loop.create_datagram_endpoint(
                lambda: _PairingProtocol(cast("PairingManager", self)),
                sock=sock,
            )
        except OSError as exc:
            log.error(
                "pairing_bind_failed",
                addr=bind_addr,
                port=PAIR_UDP_PORT,
                error=str(exc),
            )
            return False
        self._transport = transport  # type: ignore[assignment]
        self._protocol = protocol  # type: ignore[assignment]
        log.info("pairing_bind_ok", addr=bind_addr, port=PAIR_UDP_PORT)
        return True

    def _close_socket(self) -> None:
        if self._transport is not None:
            try:
                self._transport.close()
            except Exception:
                pass
        self._transport = None
        self._protocol = None

    async def _expire_at(self, when_monotonic_ns: int) -> None:
        """Sleep until `when_monotonic_ns` then close the window if
        still open. Monotonic clock so an NTP step during the Accept
        window does not wake the expiry task early or late."""
        delay = max(0.0, (when_monotonic_ns - time.monotonic_ns()) / 1e9)
        try:
            await asyncio.sleep(delay)
        except asyncio.CancelledError:
            return
        # Only close if this is still the active window (operator may
        # have already closed it or reopened a new one).
        async with self._lock:
            if (
                self._window is not None
                and self._window.closes_at_monotonic_ns == when_monotonic_ns
                and not self._is_window_open_locked()
            ):
                await self._publish_close_locked()

    async def _tick_while_open(self, window_monotonic_ns: int) -> None:
        """Publish `accept_window_tick` every 5 s while `window_monotonic_ns`
        is the active deadline. Lets OLED and GCS render a live countdown
        without polling REST. Tick stops automatically when the deadline
        passes, when the operator closes early, or when a new window is
        opened (the `closes_at_monotonic_ns` compare becomes false)."""
        TICK_PERIOD_S = 5.0
        while True:
            try:
                await asyncio.sleep(TICK_PERIOD_S)
            except asyncio.CancelledError:
                return
            now_ns = time.monotonic_ns()
            remaining_ns = window_monotonic_ns - now_ns
            if remaining_ns <= 0:
                return
            # Snapshot the window reference under the lock; if a newer
            # window has superseded this one, we exit. Avoid emitting a
            # tick for a stale deadline.
            async with self._lock:
                if (
                    self._window is None
                    or self._window.closes_at_monotonic_ns != window_monotonic_ns
                ):
                    return
                remaining_s = max(0, int(remaining_ns // 1_000_000_000))
                now_ms = int(time.time() * 1000)
            await _publish_pair_event(
                self._bus,
                PairingEvent(
                    kind="accept_window_tick",
                    timestamp_ms=now_ms,
                    payload={"remaining_seconds": remaining_s},
                ),
            )

    async def _publish_close_locked(self) -> None:
        """Internal close helper. Caller must hold `_lock`."""
        now_ms = int(time.time() * 1000)
        self._window = None
        self._priv = None
        self._close_socket()
        if self._expire_task is not None and not self._expire_task.done():
            self._expire_task.cancel()
            self._expire_task = None
        if self._tick_task is not None and not self._tick_task.done():
            self._tick_task.cancel()
            self._tick_task = None
        await _publish_pair_event(
            self._bus,
            PairingEvent(
                kind="accept_window_closed",
                timestamp_ms=now_ms,
                payload={},
            ),
        )
        log.info("pairing_window_closed")

    def _is_window_open_locked(self) -> bool:
        if self._window is None:
            return False
        # Authoritative freshness check uses monotonic ns. Wall-clock
        # only serves the display field `closes_at_ms` which is what
        # the operator sees on the OLED countdown and in REST snapshots.
        if self._window.closes_at_monotonic_ns > 0:
            return time.monotonic_ns() < self._window.closes_at_monotonic_ns
        # Backwards-compat for any in-flight window opened before the
        # monotonic field existed.
        return int(time.time() * 1000) < self._window.closes_at_ms
