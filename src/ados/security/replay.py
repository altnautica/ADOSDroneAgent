"""Replay attack detection using timestamp window and nonce tracking."""

from __future__ import annotations

import time
from dataclasses import dataclass, field

from ados.core.logging import get_logger

log = get_logger("replay-detector")


@dataclass
class _NonceEntry:
    """A nonce with its associated timestamp for expiry tracking."""

    nonce: str
    timestamp: float


class ReplayDetector:
    """Detects replayed commands using timestamp bounds and nonce deduplication.

    A message is valid if:
    1. Its timestamp is within `window_seconds` of current time.
    2. Its nonce has not been seen before within the window.

    Nonces are pruned periodically to prevent unbounded memory growth.
    """

    def __init__(
        self,
        window_seconds: float = 30.0,
        max_nonces: int = 10000,
    ) -> None:
        self._window = window_seconds
        self._max_nonces = max_nonces
        self._nonces: dict[str, float] = {}
        self._last_prune: float = 0.0

    @property
    def window_seconds(self) -> float:
        return self._window

    @property
    def nonce_count(self) -> int:
        return len(self._nonces)

    def check(self, timestamp: float, nonce: str) -> bool:
        """Check whether a message is valid (not replayed).

        Args:
            timestamp: The message timestamp (Unix seconds).
            nonce: A unique identifier for this message.

        Returns:
            True if the message is valid (fresh), False if it should be rejected.
        """
        now = time.time()

        # Reject messages outside the time window
        age = abs(now - timestamp)
        if age > self._window:
            log.warning("replay_timestamp_expired", age=round(age, 2), window=self._window)
            return False

        # Reject duplicate nonces
        if nonce in self._nonces:
            log.warning("replay_nonce_duplicate", nonce=nonce)
            return False

        # Store nonce
        self._nonces[nonce] = timestamp

        # Prune if needed (every window_seconds or when too many nonces)
        if (
            now - self._last_prune > self._window
            or len(self._nonces) > self._max_nonces
        ):
            self.prune()

        return True

    def prune(self) -> int:
        """Remove expired nonces outside the time window.

        Returns the number of nonces removed.
        """
        now = time.time()
        cutoff = now - self._window

        expired = [n for n, ts in self._nonces.items() if ts < cutoff]
        for n in expired:
            del self._nonces[n]

        self._last_prune = now

        if expired:
            log.debug("nonces_pruned", count=len(expired), remaining=len(self._nonces))

        return len(expired)
