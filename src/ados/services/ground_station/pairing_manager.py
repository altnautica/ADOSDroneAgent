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
    opened_at_ms: int
    closes_at_ms: int
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


def load_revocations() -> set[str]:
    """Read the revocation list. Returns an empty set on missing/bad file."""
    if not REVOCATIONS_PATH.is_file():
        return set()
    try:
        data = json.loads(REVOCATIONS_PATH.read_text(encoding="utf-8"))
        if isinstance(data, list):
            return {str(x) for x in data}
    except (OSError, ValueError):
        pass
    return set()


def save_revocations(revoked: set[str]) -> None:
    REVOCATIONS_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = REVOCATIONS_PATH.with_suffix(REVOCATIONS_PATH.suffix + ".tmp")
    tmp.write_text(json.dumps(sorted(revoked)), encoding="utf-8")
    os.chmod(tmp, 0o600)
    os.replace(str(tmp), str(REVOCATIONS_PATH))


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

    @property
    def window(self) -> AcceptWindow | None:
        return self._window

    async def _bind_socket(self, bind_addr: str = "0.0.0.0") -> bool:
        """Bring up the UDP listener. Idempotent."""
        if self._transport is not None:
            return True
        loop = asyncio.get_running_loop()
        try:
            transport, protocol = await loop.create_datagram_endpoint(
                lambda: _PairingProtocol(self),
                local_addr=(bind_addr, PAIR_UDP_PORT),
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

    async def _expire_at(self, when_ms: int) -> None:
        """Sleep until `when_ms` then close the window if still open."""
        now_ms = int(time.time() * 1000)
        delay = max(0.0, (when_ms - now_ms) / 1000.0)
        try:
            await asyncio.sleep(delay)
        except asyncio.CancelledError:
            return
        # Only close if this is still the active window (operator may
        # have already closed it or reopened a new one).
        async with self._lock:
            if (
                self._window is not None
                and self._window.closes_at_ms == when_ms
                and not self._is_window_open_locked()
            ):
                await self._publish_close_locked()

    async def _publish_close_locked(self) -> None:
        """Internal close helper. Caller must hold `_lock`."""
        now_ms = int(time.time() * 1000)
        self._window = None
        self._priv = None
        self._close_socket()
        if self._expire_task is not None and not self._expire_task.done():
            self._expire_task.cancel()
            self._expire_task = None
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
        # Schedule auto-close at the expiry deadline.
        if self._expire_task is not None and not self._expire_task.done():
            self._expire_task.cancel()
        self._expire_task = asyncio.create_task(
            self._expire_at(self._window.closes_at_ms)
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
        return int(time.time() * 1000) < self._window.closes_at_ms

    def is_window_open(self) -> bool:
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
