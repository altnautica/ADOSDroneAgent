"""Mesh-invite crypto: the bundle, the accept-window code, and the sealed blob.

The receiver seals an invite bundle to the relay's ephemeral X25519 key.
ECDH alone proves nothing about who answered: any node that heard the
relay's join request (which falls back to broadcast) can seal a bundle of
its own to that key. So the session key also binds the accept window's
code, a six-digit number the receiver shows its operator and the relay's
operator types in. An invite only opens for a relay holding that code, and
a relay only accepts an invite from a receiver that showed it.

Session key: ``prk = HMAC-SHA256(0x00 * 32, shared)`` then
``HMAC-SHA256(prk, context || 0x01)``, where ``context`` is
``b"ados-mesh-invite" || 0x00 || code``. The Rust ``ados-groundlink``
pairing crypto derives the same key; both carry the same test vector.

Wire format of a sealed invite::

    32 bytes  receiver_pubkey
    12 bytes  nonce
    N bytes   ciphertext || tag   (ChaCha20Poly1305, no associated data)
"""

from __future__ import annotations

import json
import secrets
import time
from dataclasses import dataclass

from cryptography.exceptions import InvalidTag
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

#: Fixed prefix of the session-key context.
INVITE_CONTEXT = b"ados-mesh-invite"

#: Length of the accept-window code, in decimal digits.
INVITE_CODE_DIGITS = 6


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


def new_invite_code() -> str:
    """A fresh accept-window code: six random decimal digits."""
    return f"{secrets.randbelow(10**INVITE_CODE_DIGITS):0{INVITE_CODE_DIGITS}d}"


def is_invite_code(code: str) -> bool:
    """Whether ``code`` has the accept-window code's shape."""
    return len(code) == INVITE_CODE_DIGITS and code.isascii() and code.isdigit()


def invite_context(code: str) -> bytes:
    """The session-key context for ``code``. Raises ValueError on a malformed code."""
    if not is_invite_code(code):
        raise ValueError(f"invite code must be {INVITE_CODE_DIGITS} digits")
    return INVITE_CONTEXT + b"\x00" + code.encode("ascii")


def session_key(shared: bytes, context: bytes) -> bytes:
    """Derive the 32-byte ChaCha20Poly1305 key from the ECDH shared secret."""
    h = hmac.HMAC(b"\x00" * 32, hashes.SHA256())
    h.update(shared)
    prk = h.finalize()
    h2 = hmac.HMAC(prk, hashes.SHA256())
    h2.update(context + b"\x01")
    return h2.finalize()


def generate_keypair() -> tuple[X25519PrivateKey, bytes]:
    """Return (private, public_bytes) for ECDH."""
    priv = X25519PrivateKey.generate()
    pub = priv.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    return priv, pub


def encrypt_invite(
    bundle: InviteBundle,
    receiver_priv: X25519PrivateKey,
    relay_pubkey_bytes: bytes,
    code: str,
) -> bytes:
    """Seal ``bundle`` to the relay's key under the accept window's ``code``."""
    peer_pub = X25519PublicKey.from_public_bytes(relay_pubkey_bytes)
    shared = receiver_priv.exchange(peer_pub)
    key = session_key(shared, invite_context(code))
    nonce = secrets.token_bytes(12)
    ct = ChaCha20Poly1305(key).encrypt(nonce, bundle.pack(), associated_data=None)
    receiver_pub = receiver_priv.public_key().public_bytes(
        Encoding.Raw, PublicFormat.Raw,
    )
    return receiver_pub + nonce + ct


def decrypt_invite(
    blob: bytes,
    relay_priv: X25519PrivateKey,
    code: str,
) -> InviteBundle:
    """Open an invite sealed under ``code``.

    Every way an invite can fail (short, sealed under another key or code,
    malformed body, expired) raises ValueError, so a receive loop can drop
    it and keep listening.
    """
    context = invite_context(code)
    if len(blob) < 32 + 12 + 16:
        raise ValueError("invite blob too short")
    peer_pub = X25519PublicKey.from_public_bytes(blob[:32])
    shared = relay_priv.exchange(peer_pub)
    key = session_key(shared, context)
    try:
        plaintext = ChaCha20Poly1305(key).decrypt(blob[32:44], blob[44:], associated_data=None)
    except InvalidTag as exc:
        raise ValueError("invite did not open with this key and code") from exc
    try:
        bundle = InviteBundle.unpack(plaintext)
    except (KeyError, TypeError) as exc:
        raise ValueError(f"invite bundle malformed: {exc}") from exc
    if int(time.time() * 1000) > bundle.expires_at_ms:
        raise ValueError("invite expired")
    return bundle
