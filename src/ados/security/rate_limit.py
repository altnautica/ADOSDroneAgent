"""Token bucket rate limiter with FastAPI middleware."""

from __future__ import annotations

import time
from collections.abc import Callable
from dataclasses import dataclass

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.responses import JSONResponse, Response

from ados.core.logging import get_logger

log = get_logger("rate-limiter")


@dataclass
class _Bucket:
    """A single token bucket."""

    tokens: float
    last_refill: float


class RateLimiter:
    """Token bucket rate limiter with per-key buckets.

    Each key (e.g., client IP) gets its own bucket that refills
    at `rate` tokens per second, up to a `burst` maximum.
    """

    def __init__(self, rate: float = 10.0, burst: int = 20) -> None:
        self._rate = rate
        self._burst = burst
        self._buckets: dict[str, _Bucket] = {}

    @property
    def rate(self) -> float:
        return self._rate

    @property
    def burst(self) -> int:
        return self._burst

    def allow(self, key: str = "default") -> bool:
        """Check if a request is allowed under the rate limit.

        Args:
            key: Identifier for the requester (e.g., IP address).

        Returns:
            True if the request is allowed, False if rate-limited.
        """
        now = time.monotonic()

        if key not in self._buckets:
            self._buckets[key] = _Bucket(tokens=float(self._burst), last_refill=now)

        bucket = self._buckets[key]

        # Refill tokens based on elapsed time
        elapsed = now - bucket.last_refill
        bucket.tokens = min(float(self._burst), bucket.tokens + elapsed * self._rate)
        bucket.last_refill = now

        if bucket.tokens >= 1.0:
            bucket.tokens -= 1.0
            return True

        log.warning("rate_limited", key=key)
        return False

    def reset(self, key: str | None = None) -> None:
        """Reset a specific bucket or all buckets."""
        if key is None:
            self._buckets.clear()
        elif key in self._buckets:
            del self._buckets[key]


class RateLimitMiddleware(BaseHTTPMiddleware):
    """FastAPI/Starlette middleware that applies token bucket rate limiting.

    Returns 429 Too Many Requests when a client exceeds their rate limit.
    Uses client IP address as the bucket key.
    """

    def __init__(
        self,
        app: object,
        rate: float = 10.0,
        burst: int = 20,
        trusted_proxies: list[str] | None = None,
    ) -> None:
        super().__init__(app)  # type: ignore[arg-type]
        self._limiter = RateLimiter(rate=rate, burst=burst)
        self._trusted_proxies: frozenset[str] = frozenset(trusted_proxies or [])

    def _get_client_ip(self, request: Request) -> str:
        """Extract client IP from request.

        Uses request.client.host as the primary identifier. Only trusts
        X-Forwarded-For if the direct client IP is in the trusted_proxies list.
        """
        direct_ip = request.client.host if request.client else "unknown"

        if self._trusted_proxies and direct_ip in self._trusted_proxies:
            forwarded = request.headers.get("x-forwarded-for")
            if forwarded:
                return forwarded.split(",")[0].strip()

        return direct_ip

    async def dispatch(self, request: Request, call_next: Callable) -> Response:  # type: ignore[type-arg]
        client_ip = self._get_client_ip(request)

        if not self._limiter.allow(client_ip):
            return JSONResponse(
                status_code=429,
                content={"detail": "Too many requests. Please slow down."},
            )

        response = await call_next(request)
        return response
