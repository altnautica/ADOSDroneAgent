"""Captive-portal single-use token store.

Lightweight in-memory token authority for the ground-station setup
webapp. The webapp fetches a token from
`GET /api/v1/ground-station/captive-token` on page load and attaches
it via the `X-ADOS-Captive-Key` header on destructive operations such
as factory reset.

Scope: POC protection. The token store is intentionally in-memory,
non-persistent, and resets on agent restart. The endpoint that mints
tokens is gated on the AP subnet so only hosts connected to the
ground-station hotspot can request one.

Tokens are 32 hex chars from `secrets.token_hex(16)`. Each token is
single-use: `consume()` returns True once and False on every repeat
call. `invalidate_all()` wipes the store and is called when
`/var/lib/ados/setup-complete` is written.
"""

from __future__ import annotations

import secrets
import threading
import time
from typing import Optional


class CaptiveTokenStore:
    """Thread-safe single-use token store for the setup webapp."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._tokens: dict[str, dict[str, float | bool]] = {}

    def generate(self) -> str:
        """Mint a fresh token, record creation time, return hex string."""
        token = secrets.token_hex(16)
        with self._lock:
            self._tokens[token] = {"created": time.time(), "consumed": False}
        return token

    def consume(self, token: str) -> bool:
        """Mark a token consumed. Returns True on first use, else False.

        Unknown or already-consumed tokens return False. Idempotent: a
        second consume of the same token is a no-op that returns False.
        """
        with self._lock:
            entry = self._tokens.get(token)
            if entry is None:
                return False
            if entry.get("consumed"):
                return False
            entry["consumed"] = True
            return True

    def invalidate_all(self) -> None:
        """Drop every token. Called when setup completes."""
        with self._lock:
            self._tokens.clear()


_store: Optional[CaptiveTokenStore] = None


def get_captive_token_store() -> CaptiveTokenStore:
    """Module-level singleton accessor."""
    global _store
    if _store is None:
        _store = CaptiveTokenStore()
    return _store
