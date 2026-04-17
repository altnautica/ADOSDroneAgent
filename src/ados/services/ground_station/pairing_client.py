"""Relay-side pairing client.

On a relay node the operator presses B1 on the OLED "Join mesh" screen
(or calls `POST /pair/join`). This module handles the wire side of that
gesture:

1. Generate an ephemeral X25519 keypair.
2. Resolve the receiver via mDNS on the mesh interface, or fall back to
   UDP broadcast on `bat0`.
3. Send a join request datagram to `receiver:5801` (see `pairing_manager`
   for the wire format).
4. Wait for the encrypted invite blob reply on the same socket.
5. Decrypt with our private key and persist mesh identity to disk so
   mesh_manager can bring up batman-adv on its next start.
6. Return success so the caller can publish an OLED "joined" screen and
   trigger a role transition to `relay`.

No laptop. No cloud. No QR codes. The relay needs only the shared mesh
PSK and receiver mDNS name delivered in the invite bundle.
"""

from __future__ import annotations

import asyncio
import json
import os
import socket
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

from ados.core.config import load_config
from ados.core.logging import get_logger

from .events import PairingEvent, get_pairing_event_bus
from .mdns_announce import iface_ip, resolve_receiver
from .pairing_manager import (
    InviteBundle,
    PAIR_UDP_PORT,
    decrypt_invite,
    generate_keypair,
)

log = get_logger("ground_station.pairing_client")

MESH_DIR = Path("/etc/ados/mesh")
MESH_ID_PATH = MESH_DIR / "id"
MESH_PSK_PATH = MESH_DIR / "psk.key"
RECEIVER_INFO_PATH = MESH_DIR / "receiver.json"

_WFB_KEY_DIR = Path("/etc/ados/wfb")
_WFB_RX_KEY_PATH = _WFB_KEY_DIR / "rx.key"

_DEFAULT_SERVICE = "_ados-receiver._tcp"
_BROADCAST_FALLBACK_ADDR = "255.255.255.255"


@dataclass
class JoinResult:
    ok: bool
    mesh_id: str | None = None
    receiver_host: str | None = None
    error_code: str | None = None
    error_message: str | None = None


async def _send_join_request(
    sock: socket.socket,
    device_id: str,
    pubkey: bytes,
    receiver_addr: tuple[str, int],
) -> None:
    payload = json.dumps(
        {
            "type": "join",
            "device_id": device_id,
            "pubkey_hex": pubkey.hex(),
        }
    ).encode("utf-8")
    loop = asyncio.get_running_loop()
    await loop.sock_sendto(sock, payload, receiver_addr)
    log.info(
        "pairing_join_sent",
        device_id=device_id,
        addr=f"{receiver_addr[0]}:{receiver_addr[1]}",
    )


def _persist_bundle(bundle: InviteBundle) -> None:
    """Write the decrypted bundle fields to disk.

    File layout:
        /etc/ados/mesh/id             mesh_id (one line)
        /etc/ados/mesh/psk.key        shared PSK (32 bytes, 0o600)
        /etc/ados/mesh/receiver.json  receiver mDNS hint + ports
        /etc/ados/wfb/rx.key          drone-paired wfb rx key
    """
    MESH_DIR.mkdir(parents=True, exist_ok=True)
    os.chmod(MESH_DIR, 0o755)

    # mesh id
    MESH_ID_PATH.write_text(bundle.mesh_id + "\n", encoding="utf-8")
    os.chmod(MESH_ID_PATH, 0o644)

    # mesh PSK. 0o600 is mandatory; this is the deployment secret.
    MESH_PSK_PATH.write_bytes(bundle.mesh_psk)
    os.chmod(MESH_PSK_PATH, 0o600)

    # Receiver mDNS hint. Used by wfb_relay + mesh_manager to resolve
    # the receiver on bat0 after the mesh is up.
    receiver_info: dict[str, Any] = {
        "mdns_host": bundle.receiver_mdns_host,
        "mdns_port": bundle.receiver_mdns_port,
        "issued_at_ms": bundle.issued_at_ms,
        "expires_at_ms": bundle.expires_at_ms,
    }
    RECEIVER_INFO_PATH.write_text(
        json.dumps(receiver_info), encoding="utf-8"
    )
    os.chmod(RECEIVER_INFO_PATH, 0o644)

    # WFB rx key. Same path wfb_rx.py already reads via key_mgr.
    if bundle.wfb_rx_key:
        _WFB_KEY_DIR.mkdir(parents=True, exist_ok=True)
        _WFB_RX_KEY_PATH.write_bytes(bundle.wfb_rx_key)
        os.chmod(_WFB_RX_KEY_PATH, 0o600)


async def request_join(
    receiver_host: str | None = None,
    receiver_port: int | None = None,
    timeout_s: float = 45.0,
) -> JoinResult:
    """Send a join request and wait for an invite reply.

    Args:
        receiver_host: Optional explicit hostname or IP. When omitted,
            mDNS discovery on the mesh interface decides.
        receiver_port: Optional port override. Defaults to the pairing
            UDP port.
        timeout_s: How long to wait for the reply after sending.

    Returns a JoinResult with ok=False on any failure, including mDNS
    resolve timeout, decrypt error, or invite expiry.
    """
    config = load_config()
    device_id = config.agent.device_id or "relay"
    mesh_iface = config.ground_station.mesh.bat_iface

    # Resolve the receiver. Operator can pass an explicit host (used by
    # the REST escape hatch); otherwise we scan mDNS on the mesh
    # interface. If both fail, we fall back to broadcast.
    port = receiver_port or PAIR_UDP_PORT
    receiver_addr: tuple[str, int] | None = None
    if receiver_host:
        try:
            ip = socket.gethostbyname(receiver_host)
            receiver_addr = (ip, port)
        except OSError:
            log.warning("pairing_host_resolve_failed", host=receiver_host)
    if receiver_addr is None:
        resolved = await resolve_receiver(
            _DEFAULT_SERVICE, mesh_iface, timeout=3.0,
        )
        if resolved is not None:
            receiver_addr = (resolved.ip, port)
    if receiver_addr is None:
        # Fall back to limited broadcast on bat0. The receiver's UDP
        # listener will ignore packets from outside the mesh subnet,
        # so this is safe even on shared networks.
        local_ip = iface_ip(mesh_iface)
        if local_ip is None:
            return JoinResult(
                ok=False,
                error_code="E_MESH_IFACE_DOWN",
                error_message=f"mesh interface {mesh_iface} has no IP",
            )
        receiver_addr = (_BROADCAST_FALLBACK_ADDR, port)

    priv, pub = generate_keypair()

    # Set up a single socket we use for both send and recv. Bind to
    # ephemeral port on the mesh interface so the receiver's reply has
    # a valid return path over bat0.
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    sock.setblocking(False)
    bind_ip = iface_ip(mesh_iface) or "0.0.0.0"
    try:
        sock.bind((bind_ip, 0))
    except OSError as exc:
        sock.close()
        return JoinResult(
            ok=False,
            error_code="E_BIND_FAILED",
            error_message=str(exc),
        )

    bus = get_pairing_event_bus()

    try:
        await _send_join_request(sock, device_id, pub, receiver_addr)
        loop = asyncio.get_running_loop()
        # Wait for the invite reply. Drop anything that fails to
        # decrypt (noise, wrong deployment, stale retry).
        deadline = time.monotonic() + timeout_s
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return JoinResult(
                    ok=False,
                    error_code="E_JOIN_TIMEOUT",
                    error_message="no invite reply received",
                )
            try:
                data, _src = await asyncio.wait_for(
                    loop.sock_recvfrom(sock, 4096),
                    timeout=remaining,
                )
            except asyncio.TimeoutError:
                return JoinResult(
                    ok=False,
                    error_code="E_JOIN_TIMEOUT",
                    error_message="no invite reply received",
                )
            try:
                bundle = decrypt_invite(data, priv)
            except ValueError as exc:
                log.debug("pairing_decrypt_skipped", error=str(exc))
                continue
            _persist_bundle(bundle)
            await bus.publish(
                PairingEvent(
                    kind="join_completed",
                    timestamp_ms=int(time.time() * 1000),
                    payload={
                        "mesh_id": bundle.mesh_id,
                        "receiver_host": bundle.receiver_mdns_host,
                    },
                )
            )
            log.info(
                "pairing_join_completed",
                mesh_id=bundle.mesh_id,
                receiver_host=bundle.receiver_mdns_host,
            )
            return JoinResult(
                ok=True,
                mesh_id=bundle.mesh_id,
                receiver_host=bundle.receiver_mdns_host,
            )
    finally:
        try:
            sock.close()
        except Exception:
            pass


def has_persisted_identity() -> bool:
    """True if mesh_id + psk are already on disk.

    Used by mesh_manager on relay nodes to decide whether it can start
    without prompting for a pair, and by the OLED to pick between
    "Join mesh" and "Joined" screens.
    """
    return MESH_ID_PATH.is_file() and MESH_PSK_PATH.is_file()


def clear_persisted_identity() -> None:
    """Remove mesh identity files. Used by factory reset."""
    for p in (MESH_ID_PATH, MESH_PSK_PATH, RECEIVER_INFO_PATH):
        try:
            if p.is_file():
                p.unlink()
        except OSError as exc:
            log.warning("mesh_identity_delete_failed", path=str(p), error=str(exc))
