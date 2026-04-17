"""Field-only tap-to-pair for mesh relays.

When a receiver operator opens the Accept window from the OLED, this
module listens on UDP/`bat0` for join requests, runs a Curve25519 ECDH
key exchange with each requesting relay, encrypts an invite bundle
containing everything the relay needs to join the deployment (mesh id,
shared PSK, receiver mDNS name, drone WFB key, expiry), and sends it
back. The relay writes the bundle into `/etc/ados/mesh/` and restarts
its mesh services.

No laptop. No cloud. No QR codes. Default window is 60 seconds; the
receiver operator explicitly closes it by pressing B4.

State machine (per node)::

    idle -> accept_window_open -> request_received -> approved -> completed
                     |-> closed (60s timeout or B4)

    idle -> joining -> joined
           |-> psk_mismatch | bundle_expired | revoked

Revocation list is persisted at `/etc/ados/mesh/revocations.json`
(0o600). When a revoked relay attempts to join, the request is silently
dropped and `revoked` event published.

The module provides library-level primitives only. The REST router and
OLED screens drive the lifecycle.
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import socket
import struct
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from cryptography.hazmat.primitives import hashes, hmac
from cryptography.hazmat.primitives.asymmetric.x25519 import (
    X25519PrivateKey,
    X25519PublicKey,
)
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.serialization import (
    Encoding,
    PublicFormat,
)

from ados.core.logging import get_logger

from .events import PairingEvent, get_pairing_event_bus

log = get_logger("ground_station.pairing_manager")

REVOCATIONS_PATH = Path("/etc/ados/mesh/revocations.json")
PAIR_UDP_PORT = 5801
DEFAULT_ACCEPT_WINDOW_S = 60
INVITE_TTL_S = 120
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
class InviteBundle:
    """What a relay receives on approval.

    Fields map 1:1 into /etc/ados/mesh/ paths on the relay side.
    """

    mesh_id: str
    mesh_psk: bytes  # 32 bytes
    drone_channel: int
    wfb_rx_key: bytes  # drone-paired wfb rx key material
    receiver_mdns_host: str
    receiver_mdns_port: int
    issued_at_ms: int
    expires_at_ms: int

    def pack(self) -> bytes:
        payload = {
            "mesh_id": self.mesh_id,
            "mesh_psk": self.mesh_psk.hex(),
            "drone_channel": self.drone_channel,
            "wfb_rx_key": self.wfb_rx_key.hex(),
            "receiver_mdns_host": self.receiver_mdns_host,
            "receiver_mdns_port": self.receiver_mdns_port,
            "issued_at_ms": self.issued_at_ms,
            "expires_at_ms": self.expires_at_ms,
        }
        return json.dumps(payload, sort_keys=True).encode("utf-8")

    @classmethod
    def unpack(cls, blob: bytes) -> InviteBundle:
        data = json.loads(blob.decode("utf-8"))
        return cls(
            mesh_id=data["mesh_id"],
            mesh_psk=bytes.fromhex(data["mesh_psk"]),
            drone_channel=int(data["drone_channel"]),
            wfb_rx_key=bytes.fromhex(data["wfb_rx_key"]),
            receiver_mdns_host=data["receiver_mdns_host"],
            receiver_mdns_port=int(data["receiver_mdns_port"]),
            issued_at_ms=int(data["issued_at_ms"]),
            expires_at_ms=int(data["expires_at_ms"]),
        )


@dataclass
class AcceptWindow:
    # Wall-clock timestamps kept for UI display and REST snapshots.
    # Do NOT use them for freshness checks; wall-clock can go backwards
    # on NTP step corrections and expire a valid window early or accept
    # a stale one. `closes_at_monotonic_ns` is the authoritative deadline.
    opened_at_ms: int
    closes_at_ms: int
    # Authoritative deadline for `is_window_open` and `_expire_at`.
    # Populated when the window is created.
    closes_at_monotonic_ns: int = 0
    pending: list[PendingRequest] = field(default_factory=list)
    approvals: dict[str, int] = field(default_factory=dict)  # device_id -> ts


def _hkdf_session_key(shared: bytes, context: bytes) -> bytes:
    """Derive a 32-byte ChaCha20Poly1305 key from the ECDH shared secret."""
    h = hmac.HMAC(b"\x00" * 32, hashes.SHA256())
    h.update(shared)
    prk = h.finalize()
    h2 = hmac.HMAC(prk, hashes.SHA256())
    h2.update(context + b"\x01")
    return h2.finalize()


# In-memory revocation cache. Every join request lands here first, so a
# from-disk reload per request would be a DoS vector under churn. Cache
# is refreshed on any revoke / unrevoke mutation and on a 30 s TTL; set
# `_REVOCATIONS_CACHE_TS_NS` to 0 to force a reload on next read.
_REVOCATIONS_CACHE: set[str] | None = None
_REVOCATIONS_CACHE_TS_NS: int = 0
_REVOCATIONS_CACHE_TTL_S = 30.0


def _read_revocations_from_disk() -> set[str]:
    if not REVOCATIONS_PATH.is_file():
        return set()
    try:
        data = json.loads(REVOCATIONS_PATH.read_text(encoding="utf-8"))
        if isinstance(data, list):
            return {str(x) for x in data}
    except (OSError, ValueError):
        pass
    return set()


def load_revocations() -> set[str]:
    """Return the current revocation set. Cached in memory with a 30 s
    TTL to keep `is_revoked` cheap under join churn. Mutations go
    through `save_revocations` / `revoke` / `unrevoke` which bust the
    cache explicitly."""
    global _REVOCATIONS_CACHE, _REVOCATIONS_CACHE_TS_NS
    now_ns = time.monotonic_ns()
    if _REVOCATIONS_CACHE is not None and (
        (now_ns - _REVOCATIONS_CACHE_TS_NS) < int(_REVOCATIONS_CACHE_TTL_S * 1e9)
    ):
        return set(_REVOCATIONS_CACHE)
    fresh = _read_revocations_from_disk()
    _REVOCATIONS_CACHE = fresh
    _REVOCATIONS_CACHE_TS_NS = now_ns
    return set(fresh)


def save_revocations(revoked: set[str]) -> None:
    REVOCATIONS_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = REVOCATIONS_PATH.with_suffix(REVOCATIONS_PATH.suffix + ".tmp")
    tmp.write_text(json.dumps(sorted(revoked)), encoding="utf-8")
    os.chmod(tmp, 0o600)
    os.replace(str(tmp), str(REVOCATIONS_PATH))
    # Update the cache synchronously so the next `is_revoked` call sees
    # the post-mutation state immediately, not after the TTL.
    global _REVOCATIONS_CACHE, _REVOCATIONS_CACHE_TS_NS
    _REVOCATIONS_CACHE = set(revoked)
    _REVOCATIONS_CACHE_TS_NS = time.monotonic_ns()


def revoke(device_id: str) -> None:
    rs = load_revocations()
    rs.add(device_id)
    save_revocations(rs)
    log.info("pairing_revoked", device_id=device_id)


def unrevoke(device_id: str) -> None:
    rs = load_revocations()
    rs.discard(device_id)
    save_revocations(rs)


def is_revoked(device_id: str) -> bool:
    return device_id in load_revocations()


def generate_keypair() -> tuple[X25519PrivateKey, bytes]:
    """Return (private, public_bytes) for ECDH."""
    priv = X25519PrivateKey.generate()
    pub = priv.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    return priv, pub


def encrypt_invite(
    bundle: InviteBundle,
    receiver_priv: X25519PrivateKey,
    relay_pubkey_bytes: bytes,
) -> bytes:
    """ECDH + ChaCha20Poly1305 encrypted invite bundle.

    Wire format:
        32 bytes  receiver_pubkey
        12 bytes  nonce
        N bytes   ciphertext || tag
    """
    peer_pub = X25519PublicKey.from_public_bytes(relay_pubkey_bytes)
    shared = receiver_priv.exchange(peer_pub)
    key = _hkdf_session_key(shared, b"ados-mesh-invite")
    nonce = secrets.token_bytes(12)
    cipher = ChaCha20Poly1305(key)
    ct = cipher.encrypt(nonce, bundle.pack(), associated_data=None)
    receiver_pub = receiver_priv.public_key().public_bytes(
        Encoding.Raw, PublicFormat.Raw,
    )
    return receiver_pub + nonce + ct


def decrypt_invite(
    blob: bytes,
    relay_priv: X25519PrivateKey,
) -> InviteBundle:
    """Decrypt an invite received from the receiver."""
    if len(blob) < 32 + 12 + 16:
        raise ValueError("invite blob too short")
    receiver_pub_bytes = blob[:32]
    nonce = blob[32:44]
    ct = blob[44:]
    peer_pub = X25519PublicKey.from_public_bytes(receiver_pub_bytes)
    shared = relay_priv.exchange(peer_pub)
    key = _hkdf_session_key(shared, b"ados-mesh-invite")
    cipher = ChaCha20Poly1305(key)
    plaintext = cipher.decrypt(nonce, ct, associated_data=None)
    bundle = InviteBundle.unpack(plaintext)
    now_ms = int(time.time() * 1000)
    if now_ms > bundle.expires_at_ms:
        raise ValueError("invite expired")
    return bundle


class _PairingProtocol(asyncio.DatagramProtocol):
    """UDP datagram handler for incoming relay join requests.

    Wire format (JSON one line per datagram):
        {"type": "join", "device_id": "<id>", "pubkey_hex": "<64-hex>"}

    Reply is the encrypted invite blob from `encrypt_invite`, sent by
    the receiver when `approve()` is called. The relay listens on the
    same socket for the reply.
    """

    def __init__(self, manager: "PairingManager") -> None:
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


class PairingManager:
    """Receiver-side state machine for the Accept window.

    Owned by the REST layer; one instance per process. Not a standalone
    systemd service. When the REST handler or OLED flow opens the
    window, this class binds a UDP listener on `bat0:5801` and buffers
    incoming join requests into `pending`. The OLED handler (or REST
    caller) then approves individual device_ids, which encrypts the
    invite bundle and sends it back on the same socket. The window
    auto-closes at `closes_at_ms` via a scheduled timer task.
    """

    def __init__(self) -> None:
        self._window: AcceptWindow | None = None
        self._priv: X25519PrivateKey | None = None
        self._pub: bytes = b""
        self._bus = get_pairing_event_bus()
        self._lock = asyncio.Lock()
        self._transport: asyncio.DatagramTransport | None = None
        self._protocol: _PairingProtocol | None = None
        self._expire_task: asyncio.Task | None = None
        self._tick_task: asyncio.Task | None = None

    @property
    def window(self) -> AcceptWindow | None:
        return self._window

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
                lambda: _PairingProtocol(self),
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
            await self._bus.publish(
                PairingEvent(
                    kind="accept_window_tick",
                    timestamp_ms=now_ms,
                    payload={"remaining_seconds": remaining_s},
                )
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
        await self._bus.publish(
            PairingEvent(
                kind="accept_window_closed",
                timestamp_ms=now_ms,
                payload={},
            )
        )
        log.info("pairing_window_closed")

    async def open_window(
        self,
        duration_s: int = DEFAULT_ACCEPT_WINDOW_S,
    ) -> AcceptWindow:
        async with self._lock:
            if self._window is not None and self._is_window_open_locked():
                return self._window
            self._priv, self._pub = generate_keypair()
            now_ms = int(time.time() * 1000)
            self._window = AcceptWindow(
                opened_at_ms=now_ms,
                closes_at_ms=now_ms + duration_s * 1000,
                closes_at_monotonic_ns=(
                    time.monotonic_ns() + duration_s * 1_000_000_000
                ),
            )
            await self._bus.publish(
                PairingEvent(
                    kind="accept_window_opened",
                    timestamp_ms=now_ms,
                    payload={"duration_s": duration_s},
                )
            )
            log.info("pairing_window_opened", duration_s=duration_s)
        # Bind the UDP listener outside the lock so a slow bind does
        # not stall other acquirers. If the bind fails, close the
        # window and raise so the REST caller sees the error instead
        # of being left with an open-but-unreachable accept state.
        bound = await self._bind_socket()
        if not bound:
            async with self._lock:
                if self._window is not None:
                    await self._publish_close_locked()
            raise RuntimeError(
                f"pairing UDP bind failed on port {PAIR_UDP_PORT}"
            )
        # Schedule auto-close at the expiry deadline + a 5 s tick task
        # so OLED and GCS can render a live countdown without polling.
        if self._expire_task is not None and not self._expire_task.done():
            self._expire_task.cancel()
        self._expire_task = asyncio.create_task(
            self._expire_at(self._window.closes_at_monotonic_ns)
        )
        if self._tick_task is not None and not self._tick_task.done():
            self._tick_task.cancel()
        self._tick_task = asyncio.create_task(
            self._tick_while_open(self._window.closes_at_monotonic_ns)
        )
        return self._window

    async def close_window(self) -> None:
        async with self._lock:
            if self._window is None:
                return
            await self._publish_close_locked()

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

    async def is_window_open(self) -> bool:
        """Thread-safe public check. Acquires `_lock` so callers that then
        mutate (close, approve) see a consistent snapshot. The sync variant
        `_is_window_open_locked` stays available for call sites that are
        already inside `_lock`.
        """
        async with self._lock:
            return self._is_window_open_locked()

    async def submit_request(
        self,
        device_id: str,
        relay_pubkey: bytes,
        remote_addr: tuple[str, int],
    ) -> bool:
        """Record an incoming join request. Returns False if rejected.

        Revocation is checked first (before the lock) because the
        revocation set is file-backed and we want to short-circuit on
        a banned device without holding the state lock. The rest of the
        window-open check and the pending-list mutation happen under
        the lock so the window cannot close out from under us between
        the check and the append.
        """
        if is_revoked(device_id):
            await self._bus.publish(
                PairingEvent(
                    kind="revoked",
                    timestamp_ms=int(time.time() * 1000),
                    payload={"device_id": device_id},
                )
            )
            return False
        async with self._lock:
            if not self._is_window_open_locked():
                return False
            if self._window is None:
                return False
            existing = {r.device_id for r in self._window.pending}
            if device_id not in existing:
                self._window.pending.append(
                    PendingRequest(
                        device_id=device_id,
                        relay_pubkey=relay_pubkey,
                        remote_addr=remote_addr,
                        received_at_ms=int(time.time() * 1000),
                    )
                )
                await self._bus.publish(
                    PairingEvent(
                        kind="join_request_received",
                        timestamp_ms=int(time.time() * 1000),
                        payload={"device_id": device_id},
                    )
                )
                log.info("pairing_join_request", device_id=device_id)
        return True

    async def approve(
        self,
        device_id: str,
        bundle: InviteBundle,
    ) -> bytes | None:
        """Build the encrypted invite and send it back to the relay.

        Returns the opaque blob so the REST caller can also log or display
        it. Also writes the blob onto the UDP socket addressed to the
        relay's remote_addr so the field-only OLED flow completes without
        any GCS involvement. Returns None if the device_id is not pending
        or the window is closed.
        """
        async with self._lock:
            if self._window is None or self._priv is None:
                return None
            if not self._is_window_open_locked():
                return None
            match = next(
                (r for r in self._window.pending if r.device_id == device_id),
                None,
            )
            if match is None:
                return None
            blob = encrypt_invite(bundle, self._priv, match.relay_pubkey)
            self._window.approvals[device_id] = int(time.time() * 1000)
            remote_addr = match.remote_addr
            await self._bus.publish(
                PairingEvent(
                    kind="join_approved",
                    timestamp_ms=int(time.time() * 1000),
                    payload={"device_id": device_id},
                )
            )
            log.info("pairing_join_approved", device_id=device_id)
        # Send outside the lock so a slow sendto does not stall other
        # state transitions. The socket is process-wide; UDP is lossy
        # so we transmit twice with a 100 ms gap to survive a single
        # dropped packet without forcing the operator to press the
        # button again. The relay's receive loop ignores duplicates
        # that fail to decrypt, so double-sending is safe.
        if self._transport is not None:
            for attempt in range(2):
                try:
                    self._transport.sendto(blob, remote_addr)
                    log.info(
                        "pairing_invite_sent",
                        device_id=device_id,
                        addr=f"{remote_addr[0]}:{remote_addr[1]}",
                        attempt=attempt + 1,
                    )
                except Exception as exc:
                    log.warning(
                        "pairing_invite_send_failed",
                        device_id=device_id,
                        attempt=attempt + 1,
                        error=str(exc),
                    )
                    break
                if attempt == 0:
                    await asyncio.sleep(0.1)
        else:
            log.warning(
                "pairing_invite_no_socket",
                device_id=device_id,
            )
        return blob

    async def snapshot(self) -> dict[str, Any]:
        async with self._lock:
            if self._window is None:
                return {"open": False}
            return {
                "open": self._is_window_open_locked(),
                "opened_at_ms": self._window.opened_at_ms,
                "closes_at_ms": self._window.closes_at_ms,
                "pending": [
                    {
                        "device_id": r.device_id,
                        "received_at_ms": r.received_at_ms,
                        "remote_ip": r.remote_addr[0],
                    }
                    for r in self._window.pending
                ],
                "approvals": dict(self._window.approvals),
            }


# Process-local singleton so the REST router, OLED menu, and socket
# listener all see the same state machine.
_manager: PairingManager | None = None


def get_pairing_manager() -> PairingManager:
    global _manager
    if _manager is None:
        _manager = PairingManager()
    return _manager
