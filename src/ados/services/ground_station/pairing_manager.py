"""Field-only tap-to-pair for mesh relays.

Disambiguation: this module is the mesh-fabric tap-to-pair manager
(relay ↔ receiver invite bundle exchange over UDP/bat0). It is NOT
the WFB radio-link pair flow — that lives in the native ``ados-groundlink``
service (``pair_state``) and the supervisor's bind keys. Different concern,
different transport.

When a receiver operator opens the Accept window from the OLED, this
module listens on UDP/`bat0` for join requests, runs a Curve25519 ECDH
key exchange with each requesting relay, encrypts an invite bundle
containing everything the relay needs to join the deployment (mesh id,
shared PSK, receiver mDNS name, drone WFB key, expiry), and sends it
back. The relay writes the bundle into `/etc/ados/mesh/` and restarts
its mesh services.

Each window carries a six-digit code the receiver shows its operator.
The relay's operator enters it on join, and the invite key binds it (see
`invite_crypto`), so neither side accepts a peer that has not seen it.

No laptop. No cloud. No QR codes. Default window is 60 seconds; the
receiver operator explicitly closes it by pressing B4.

State machine (per node)::

    idle -> accept_window_open -> request_received -> approved -> completed
                     |-> closed (60s timeout or B4)

    idle -> joining -> joined
           |-> psk_mismatch | bundle_expired | revoked

Revocation list is persisted at `/etc/ados/mesh/revocations.json`
(0o600). When a revoked relay attempts to join, the request is silently
dropped and `revoked` event published. A new revocation also adds the
relay's mesh MAC (so the receiver stops presenting it as a relay), rotates
the mesh PSK and mesh id, re-seals the new bundle to every relay still
paired, and restarts `ados-batman` (see `mesh_rekey`).

The module provides library-level primitives only. The REST router and
OLED screens drive the lifecycle.
"""

from __future__ import annotations

import asyncio
import json
import os
import time
from pathlib import Path
from typing import Any

from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey

from ados.core.logging import get_logger
from ados.core.paths import MESH_ID_PATH, MESH_PSK_PATH, MESH_REVOCATIONS_JSON

from . import mesh_rekey
from ._pairing_window import (
    MESH_IFACE as MESH_IFACE,
)
from ._pairing_window import (
    PAIR_UDP_PORT,
    AcceptWindow,
    PendingRequest,
    _PairingProtocol,
    _PairingSocketMixin,
)
from ._pairing_window import (
    _resolve_bat0_ip_or_fallback as _resolve_bat0_ip_or_fallback,
)
from .events import PairingEvent, get_pairing_event_bus
from .invite_crypto import (
    InviteBundle,
    encrypt_invite,
    generate_keypair,
    new_invite_code,
)
from .pair_journal import publish_pair_event as _publish_pair_event

log = get_logger("ground_station.pairing_manager")

REVOCATIONS_PATH = MESH_REVOCATIONS_JSON
DEFAULT_ACCEPT_WINDOW_S = 60
INVITE_TTL_S = 120


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


def mesh_identity_paths() -> tuple[Path, Path]:
    """The (PSK, mesh id) files the receiver's mesh bring-up reads: the
    configured `ground_station.mesh.shared_key_path`, else the default."""
    psk_path = MESH_PSK_PATH
    try:
        from ados.core.config import load_config

        psk_path = Path(load_config().ground_station.mesh.shared_key_path)
    except Exception as exc:  # noqa: BLE001 — an unreadable config keeps the default
        log.warning("pairing_config_unreadable_using_default_psk_path", error=str(exc))
    return psk_path, MESH_ID_PATH


def revoke(
    device_id: str,
    *,
    invite_channel: mesh_rekey.InviteChannel | None = None,
) -> bool:
    """Add ``device_id`` to the revocation set and re-key the mesh.

    Returns True when this call revoked it, False when it was already revoked
    (nothing written, no re-key). On a new revocation the relay's mesh MAC is
    listed too when known, the PSK and mesh id are rotated, the new bundle is
    re-sealed to every relay still paired, and `ados-batman` is restarted.
    """
    rs = load_revocations()
    if device_id in rs:
        return False
    mesh_dir = REVOCATIONS_PATH.parent
    relays = mesh_rekey.load_paired_relays(mesh_dir)
    target = next((r for r in relays if r.device_id == device_id), None)
    rs.add(device_id)
    if target is not None:
        mac = target.mac or mesh_rekey.arp_mac(target.ip)
        if mac:
            rs.add(mac.lower())
    save_revocations(rs)
    log.info("pairing_revoked", device_id=device_id)

    remaining = [r for r in relays if r.device_id not in rs]
    _rekey_mesh(mesh_dir, remaining, invite_channel or mesh_rekey.UdpInviteChannel())
    return True


def _rekey_mesh(
    mesh_dir: Path,
    remaining: list[mesh_rekey.PairedRelay],
    channel: mesh_rekey.InviteChannel,
) -> None:
    """Rotate the PSK + mesh id, re-seal the new bundle to ``remaining`` and
    restart the batman-adv bring-up. Each step is logged; a failed step does
    not undo the revocation."""
    psk_path, mesh_id_path = mesh_identity_paths()
    try:
        mesh_rekey.rotate_identity(psk_path, mesh_id_path)
    except OSError as exc:
        log.error("mesh_identity_rotation_failed", error=str(exc))
        return
    try:
        mesh_rekey.save_paired_relays(mesh_dir, remaining)
    except OSError as exc:
        log.warning("paired_relays_write_failed", error=str(exc))
    # Imported here: the daemon imports this module, and owns the bundle
    # assembly (mesh id, PSK, drone rx key, receiver host and port).
    from . import pairing_daemon

    bundle = pairing_daemon._build_invite_bundle()
    if bundle is None:
        log.error("mesh_reseal_skipped_no_bundle")
    else:
        mesh_rekey.reseal(bundle, remaining, channel)
    mesh_rekey.restart_batman()


def unrevoke(device_id: str) -> None:
    rs = load_revocations()
    rs.discard(device_id)
    save_revocations(rs)


def is_revoked(device_id: str) -> bool:
    return device_id in load_revocations()


class PairingManager(_PairingSocketMixin):
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
                code=new_invite_code(),
                closes_at_monotonic_ns=(
                    time.monotonic_ns() + duration_s * 1_000_000_000
                ),
            )
            await _publish_pair_event(
                self._bus,
                PairingEvent(
                    kind="accept_window_opened",
                    timestamp_ms=now_ms,
                    payload={"duration_s": duration_s},
                ),
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
            await _publish_pair_event(
                self._bus,
                PairingEvent(
                    kind="revoked",
                    timestamp_ms=int(time.time() * 1000),
                    payload={"device_id": device_id},
                ),
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
                await _publish_pair_event(
                    self._bus,
                    PairingEvent(
                        kind="join_request_received",
                        timestamp_ms=int(time.time() * 1000),
                        payload={"device_id": device_id},
                    ),
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
            blob = encrypt_invite(
                bundle, self._priv, match.relay_pubkey, self._window.code,
            )
            self._window.approvals[device_id] = int(time.time() * 1000)
            remote_addr = match.remote_addr
            paired = mesh_rekey.PairedRelay(
                device_id=device_id,
                relay_pubkey_hex=match.relay_pubkey.hex(),
                ip=remote_addr[0],
                port=int(remote_addr[1]),
                code=self._window.code,
            )
            await _publish_pair_event(
                self._bus,
                PairingEvent(
                    kind="join_approved",
                    timestamp_ms=int(time.time() * 1000),
                    payload={"device_id": device_id},
                ),
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
        # Remember the relay so a later revocation can re-seal the rotated
        # bundle to it. Best-effort: approval already happened.
        paired.mac = mesh_rekey.arp_mac(paired.ip)
        try:
            mesh_rekey.record_paired_relay(REVOCATIONS_PATH.parent, paired)
        except OSError as exc:
            log.warning("paired_relay_record_failed", device_id=device_id, error=str(exc))
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
                "code": self._window.code,
            }


# Process-local singleton so the REST router, OLED menu, and socket
# listener all see the same state machine.
_manager: PairingManager | None = None


def get_pairing_manager() -> PairingManager:
    global _manager
    if _manager is None:
        _manager = PairingManager()
    return _manager
