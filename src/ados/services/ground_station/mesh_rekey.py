"""Mesh re-key on relay revocation.

Revoking a relay must remove it from the mesh fabric, not only refuse its next
join: a revoked relay that already holds the deployment PSK could otherwise
keep joining the 802.11s mesh. On revoke the receiver therefore rotates the PSK
and the mesh id, re-seals the new invite bundle to every relay that is still
paired (over the same unicast UDP invite channel the join reply uses), and
restarts the batman-adv bring-up so the receiver itself moves to the new
identity.

Each approved relay is recorded in ``paired-relays.json`` (0600, next to the
revocation list): its device id, the X25519 public key and the accept-window
code its invite was sealed under, its mesh address, and its mesh MAC when the
neighbour table resolved one.
"""

from __future__ import annotations

import json
import os
import secrets
import socket
import subprocess
from collections.abc import Callable
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Protocol

from ados.core.logging import get_logger

from .invite_crypto import InviteBundle, encrypt_invite, generate_keypair

log = get_logger("ground_station.mesh_rekey")

PAIRED_RELAYS_FILE = "paired-relays.json"
BATMAN_UNIT = "ados-batman"
MESH_IFACE = "bat0"
ARP_TABLE = Path("/proc/net/arp")
_SYSTEMCTL_TIMEOUT_S = 30.0


@dataclass
class PairedRelay:
    """One approved relay, as the receiver needs it to re-seal an invite."""

    device_id: str
    relay_pubkey_hex: str
    ip: str
    port: int
    code: str
    mac: str | None = None


class InviteChannel(Protocol):
    """Delivers a sealed invite blob to a relay's pairing address."""

    def send(self, blob: bytes, addr: tuple[str, int]) -> None: ...


class UdpInviteChannel:
    """Unicast UDP to the relay's pairing address, sent twice to survive a
    single dropped datagram (a relay ignores a duplicate it already opened)."""

    def send(self, blob: bytes, addr: tuple[str, int]) -> None:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            for _ in range(2):
                sock.sendto(blob, addr)


def _relays_path(mesh_dir: Path) -> Path:
    return mesh_dir / PAIRED_RELAYS_FILE


def load_paired_relays(mesh_dir: Path) -> list[PairedRelay]:
    """The recorded relays. A missing or malformed file is an empty list."""
    path = _relays_path(mesh_dir)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    if not isinstance(data, list):
        return []
    relays: list[PairedRelay] = []
    for item in data:
        try:
            relays.append(PairedRelay(**item))
        except TypeError:
            continue
    return relays


def save_paired_relays(mesh_dir: Path, relays: list[PairedRelay]) -> None:
    """Persist the relay list atomically, owner-only (it holds window codes)."""
    mesh_dir.mkdir(parents=True, exist_ok=True)
    path = _relays_path(mesh_dir)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps([asdict(r) for r in relays], sort_keys=True), encoding="utf-8")
    os.chmod(tmp, 0o600)
    os.replace(str(tmp), str(path))


def record_paired_relay(mesh_dir: Path, relay: PairedRelay) -> None:
    """Add or replace (by device id) one approved relay."""
    relays = [r for r in load_paired_relays(mesh_dir) if r.device_id != relay.device_id]
    relays.append(relay)
    save_paired_relays(mesh_dir, relays)


def arp_mac(ip: str, iface: str = MESH_IFACE, arp_table: Path = ARP_TABLE) -> str | None:
    """The lowercase MAC the neighbour table holds for ``ip`` on ``iface``."""
    try:
        lines = arp_table.read_text(encoding="utf-8").splitlines()[1:]
    except OSError:
        return None
    for line in lines:
        cols = line.split()
        if len(cols) != 6:
            continue
        addr, _hw_type, flags, mac, _mask, dev = cols
        if addr == ip and dev == iface and flags != "0x0" and mac != "00:00:00:00:00:00":
            return mac.lower()
    return None


def rotate_identity(psk_path: Path, mesh_id_path: Path) -> str:
    """Write a fresh 32-byte PSK (0600) and a fresh ``ados-<8 hex>`` mesh id
    (0644). Returns the new mesh id; the key is never logged."""
    psk_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = psk_path.with_suffix(psk_path.suffix + ".tmp")
    tmp.write_bytes(secrets.token_bytes(32))
    os.chmod(tmp, 0o600)
    os.replace(str(tmp), str(psk_path))

    mesh_id = "ados-" + secrets.token_hex(4)
    mesh_id_path.parent.mkdir(parents=True, exist_ok=True)
    mesh_id_path.write_text(mesh_id + "\n", encoding="utf-8")
    os.chmod(mesh_id_path, 0o644)
    log.info("mesh_identity_rotated", mesh_id=mesh_id)
    return mesh_id


def reseal(bundle: InviteBundle, relays: list[PairedRelay], channel: InviteChannel) -> list[str]:
    """Seal ``bundle`` to each relay's recorded key under its recorded code and
    send it. Returns the device ids it was delivered to; a relay whose record
    cannot seal or whose send fails is logged and skipped."""
    sent: list[str] = []
    for relay in relays:
        try:
            priv, _pub = generate_keypair()
            blob = encrypt_invite(bundle, priv, bytes.fromhex(relay.relay_pubkey_hex), relay.code)
            channel.send(blob, (relay.ip, relay.port))
        except (OSError, ValueError) as exc:
            log.warning("mesh_reseal_failed", device_id=relay.device_id, error=str(exc))
            continue
        sent.append(relay.device_id)
    log.info("mesh_resealed", relays=len(sent))
    return sent


def restart_batman(run: Callable[..., subprocess.CompletedProcess] = subprocess.run) -> bool:
    """Restart the batman-adv bring-up so it re-reads the rotated identity."""
    try:
        result = run(
            ["systemctl", "restart", BATMAN_UNIT],
            capture_output=True,
            timeout=_SYSTEMCTL_TIMEOUT_S,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        log.error("mesh_batman_restart_failed", error=str(exc))
        return False
    if result.returncode != 0:
        log.error("mesh_batman_restart_failed", returncode=result.returncode)
        return False
    return True
