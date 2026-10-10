"""Relay side of the mesh re-key on revocation.

When the receiver revokes a relay it rotates the mesh PSK and mesh id and
re-seals the new invite bundle to every relay still paired (see
``mesh_rekey``). It seals to the X25519 public key and the accept-window code
the relay joined with, and sends the blob over unicast UDP to the address the
join request came from.

So after a join the relay keeps three things: its X25519 private key, the
window code, and the UDP port its join socket used. They live in
``relay-rekey.json`` (0600) in the mesh state dir, together with the
``issued_at_ms`` of the bundle last applied. A listener on that port opens
each datagram with the key and code; a bundle that opens and is newer than
the last applied one is persisted and ``ados-batman`` is restarted, the same
restart the receiver performs. Anything else is logged and dropped; the
receiver sends each re-sealed bundle twice, so the second copy is ignored as
not newer.
"""

from __future__ import annotations

import asyncio
import json
import os
from collections.abc import Callable
from dataclasses import asdict, dataclass
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from cryptography.hazmat.primitives.serialization import (
    Encoding,
    NoEncryption,
    PrivateFormat,
)

from ados.core.logging import get_logger

from .invite_crypto import InviteBundle, decrypt_invite

log = get_logger("ground_station.relay_rekey")

REKEY_STATE_FILE = "relay-rekey.json"
_LISTEN_HOST = "0.0.0.0"


@dataclass
class RelayRekeyState:
    """What the relay needs to open a bundle the receiver re-seals to it."""

    priv_hex: str
    code: str
    port: int
    issued_at_ms: int

    @classmethod
    def from_join(
        cls, priv: X25519PrivateKey, code: str, port: int, bundle: InviteBundle
    ) -> RelayRekeyState:
        raw = priv.private_bytes(Encoding.Raw, PrivateFormat.Raw, NoEncryption())
        return cls(priv_hex=raw.hex(), code=code, port=port, issued_at_ms=bundle.issued_at_ms)

    def private_key(self) -> X25519PrivateKey:
        return X25519PrivateKey.from_private_bytes(bytes.fromhex(self.priv_hex))


def atomic_write(path: Path, data: bytes, mode: int) -> None:
    """Write ``data`` to ``path`` via a tmp file and ``os.replace``, with
    ``mode`` set before the rename so the file never exists more open."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_bytes(data)
    os.chmod(tmp, mode)
    os.replace(str(tmp), str(path))


def _state_path(mesh_dir: Path) -> Path:
    return mesh_dir / REKEY_STATE_FILE


def save_state(mesh_dir: Path, state: RelayRekeyState) -> None:
    """Persist the re-key state owner-only; it holds a private key."""
    atomic_write(
        _state_path(mesh_dir),
        json.dumps(asdict(state), sort_keys=True).encode("utf-8"),
        0o600,
    )


def load_state(mesh_dir: Path) -> RelayRekeyState | None:
    """The stored state, or None when missing or malformed."""
    try:
        data = json.loads(_state_path(mesh_dir).read_text(encoding="utf-8"))
        state = RelayRekeyState(**data)
        state.private_key()
    except (OSError, ValueError, TypeError):
        return None
    return state


class RelayRekeyReceiver:
    """Opens and applies re-sealed bundles for one stored join."""

    def __init__(
        self,
        mesh_dir: Path,
        state: RelayRekeyState,
        persist: Callable[[InviteBundle], None],
        restart: Callable[[], bool],
    ) -> None:
        self._mesh_dir = mesh_dir
        self._state = state
        self._priv = state.private_key()
        self._persist = persist
        self._restart = restart

    @property
    def state(self) -> RelayRekeyState:
        return self._state

    def handle(self, blob: bytes, src: tuple[str, int]) -> bool:
        """Apply ``blob`` if it opens and is newer. True when applied."""
        try:
            bundle = decrypt_invite(blob, self._priv, self._state.code)
        except ValueError as exc:
            log.warning("relay_rekey_unreadable", src=src[0], error=str(exc))
            return False
        if bundle.issued_at_ms <= self._state.issued_at_ms:
            log.info(
                "relay_rekey_not_newer",
                issued_at_ms=bundle.issued_at_ms,
                applied_at_ms=self._state.issued_at_ms,
            )
            return False
        try:
            self._persist(bundle)
        except OSError as exc:
            log.error("relay_rekey_persist_failed", error=str(exc))
            return False
        self._state = RelayRekeyState(
            priv_hex=self._state.priv_hex,
            code=self._state.code,
            port=self._state.port,
            issued_at_ms=bundle.issued_at_ms,
        )
        try:
            save_state(self._mesh_dir, self._state)
        except OSError as exc:
            log.warning("relay_rekey_state_write_failed", error=str(exc))
        log.info("relay_rekey_applied", mesh_id=bundle.mesh_id, src=src[0])
        self._restart()
        return True


class _Protocol(asyncio.DatagramProtocol):
    def __init__(self, receiver: RelayRekeyReceiver) -> None:
        self._receiver = receiver

    def datagram_received(self, data: bytes, addr: tuple[str, int]) -> None:
        self._receiver.handle(data, addr)


class RelayRekeyListener:
    """UDP listener on the port the receiver recorded for this relay."""

    def __init__(self, receiver: RelayRekeyReceiver, host: str = _LISTEN_HOST) -> None:
        self._receiver = receiver
        self._host = host
        self._transport: asyncio.DatagramTransport | None = None

    async def start(self) -> None:
        loop = asyncio.get_running_loop()
        port = self._receiver.state.port
        self._transport, _ = await loop.create_datagram_endpoint(
            lambda: _Protocol(self._receiver),
            local_addr=(self._host, port),
        )
        log.info("relay_rekey_listening", port=port)

    def stop(self) -> None:
        if self._transport is not None:
            self._transport.close()
            self._transport = None
