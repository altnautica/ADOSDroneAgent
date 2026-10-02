"""Self-contained WebSocket auth ticket verification.

The native control surface (``ados-control``) mints these tickets and the
native MAVLink-router proxy validates them; this module lets the residual
Python WebSocket routes (PIC / uplink / mesh events, vision detections,
Cloudflare-tunnel logs) verify the SAME self-contained ticket, so the agent
needs only one mint and no shared ticket store.

A ticket is ``v2|<scope>|<issued_at>|<expires_at>|<nonce_hex>|<sig_hex>``
where the signature is
``HMAC-SHA256(K, "v2|<scope>|<issued_at>|<expires_at>|<nonce_hex>")`` and the
key ``K = HMAC-SHA256(api_key, b"ados-ws-ticket-v1")`` is derived from the
agent's pairing key under a fixed domain-separation label. This mirrors the
Rust ``ados_protocol::ws_ticket`` byte-for-byte (the cross-language vector is
pinned in both test suites); if one side changes, the other must change in
lockstep or cross-language tickets silently stop verifying.

A ticket is single use: a successful :func:`verify_ticket` records its nonce,
and the same ticket is refused for as long as it could otherwise still verify.
The record is per process, like the Rust verifiers'.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import os
import secrets
import threading
import time
from collections import OrderedDict
from pathlib import Path

# Domain-separation label mixed into the pairing key. MUST equal the Rust
# ``ados_protocol::ws_ticket::TICKET_KEY_LABEL``.
_LABEL = b"ados-ws-ticket-v1"

DEFAULT_TTL_SECONDS = 30
MAX_TTL_SECONDS = 120
SCOPE_MAVLINK_WS = "gs.mavlink_ws"

_DEFAULT_PAIRING_PATH = "/etc/ados/pairing.json"

# Spent nonces: nonce -> unix second after which it may be forgotten. Bounded,
# oldest evicted first.
_SPENT_CAPACITY = 4096
_spent: OrderedDict[str, int] = OrderedDict()
_spent_lock = threading.Lock()


def _spend(nonce: str, now_s: int, expires_at: int) -> bool:
    """Record ``nonce`` as spent. False when it already was."""
    with _spent_lock:
        forget_at = _spent.get(nonce)
        if forget_at is not None and forget_at > now_s:
            return False
        if len(_spent) >= _SPENT_CAPACITY:
            for stale in [n for n, f in _spent.items() if f <= now_s]:
                del _spent[stale]
        while len(_spent) >= _SPENT_CAPACITY:
            _spent.popitem(last=False)
        _spent[nonce] = max(now_s + MAX_TTL_SECONDS, expires_at + 1)
        return True


def _derive_key(api_key: str) -> bytes:
    """Derive the ticket key from the pairing ``api_key`` under the label."""
    return hmac.new(api_key.encode(), _LABEL, hashlib.sha256).digest()


def _sign(key: bytes, payload: str) -> str:
    return hmac.new(key, payload.encode(), hashlib.sha256).hexdigest()


def mint_ticket(
    scope: str,
    *,
    api_key: str,
    ttl_seconds: int = DEFAULT_TTL_SECONDS,
    now: int | None = None,
    nonce: str | None = None,
) -> str:
    """Mint a ticket string for ``scope``. Production minting is the native Rust
    control surface (``ados-control``); this mirror exists for tests and the
    cross-language parity vector, and is symmetric with [`verify_ticket`]."""
    issued = int(now if now is not None else time.time())
    expires_at = issued + ttl_seconds
    nonce_hex = nonce if nonce is not None else secrets.token_hex(16)
    payload = f"v2|{scope}|{issued}|{expires_at}|{nonce_hex}"
    sig = _sign(_derive_key(api_key), payload)
    return f"{payload}|{sig}"


def verify_ticket(
    token: str,
    *,
    expected_scope: str,
    api_key: str,
    now: int | None = None,
) -> bool:
    """Return True iff ``token`` is a valid, unspent ticket for
    ``expected_scope`` under ``api_key`` and is not expired, and spend it.
    Authenticity (HMAC) is checked in constant time before scope, expiry and
    the spent-nonce record, matching the Rust verifier's order."""
    parts = token.split("|")
    if len(parts) != 6 or parts[0] != "v2":
        return False
    scope = parts[1]
    nonce = parts[4]
    if len(nonce) != 32 or any(c not in "0123456789abcdefABCDEF" for c in nonce):
        return False
    try:
        int(parts[2])
        expires_at = int(parts[3])
    except ValueError:
        return False
    # Recompute over the EXACT signed substring (the first five pipe fields),
    # so a reformat never drifts from what was signed.
    payload = "|".join(parts[:5])
    expected_sig = _sign(_derive_key(api_key), payload)
    if not hmac.compare_digest(parts[5], expected_sig):
        return False
    if scope != expected_scope:
        return False
    now_s = int(now if now is not None else time.time())
    if now_s >= expires_at:
        return False
    return _spend(nonce, now_s, expires_at)


def load_pairing_api_key(path: str | os.PathLike[str] | None = None) -> str | None:
    """Read the pairing ``api_key`` from ``pairing.json`` (the same canonical
    source the Rust daemons derive the ticket key from), honouring
    ``ADOS_PAIRING_JSON``. Returns the key only when ``paired`` is true and the
    key is non-empty, else None."""
    p = Path(
        path
        or os.environ.get("ADOS_PAIRING_JSON", _DEFAULT_PAIRING_PATH)
    )
    try:
        data = json.loads(p.read_text())
    except Exception:
        return None
    if isinstance(data, dict) and data.get("paired") and data.get("api_key"):
        return str(data["api_key"])
    return None


__all__ = [
    "DEFAULT_TTL_SECONDS",
    "MAX_TTL_SECONDS",
    "SCOPE_MAVLINK_WS",
    "mint_ticket",
    "verify_ticket",
    "load_pairing_api_key",
]
