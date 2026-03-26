"""HMAC signing and replay protection middleware.

Verifies POST/PUT/DELETE requests using:
1. Timestamp freshness (X-Timestamp header, reject if > 300s old)
2. Nonce uniqueness (X-Nonce header, reject if seen before)
3. HMAC-SHA256 signature (X-HMAC-Signature header, verify timestamp+body)

Opt-in: Only active when config.security.hmac_enabled = True.
When disabled, all requests pass through unchanged (backwards compatible).
"""

from __future__ import annotations

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.responses import JSONResponse

from ados.core.logging import get_logger
from ados.security.hmac_signing import HmacSigner
from ados.security.replay import ReplayDetector

log = get_logger("security-middleware")

# Routes exempt from HMAC verification (even when enabled)
EXEMPT_ROUTES = {
    "/",
    "/docs",
    "/openapi.json",
    "/api/pairing/claim",
    "/api/pairing/status",
}

# Only verify mutating methods
VERIFIED_METHODS = {"POST", "PUT", "DELETE", "PATCH"}


class SecurityMiddleware(BaseHTTPMiddleware):
    """HMAC signing + replay protection for command routes."""

    def __init__(self, app, enabled: bool = False, secret: str = "") -> None:
        super().__init__(app)
        self._enabled = enabled and bool(secret)
        self._signer = None
        self._detector = None

        if self._enabled:
            secret_bytes = secret.encode("utf-8")
            if len(secret_bytes) >= 16:
                self._signer = HmacSigner(secret_bytes)
                self._detector = ReplayDetector(window_seconds=300.0, max_nonces=50000)
                log.info("security_middleware_enabled")
            else:
                log.warning("hmac_secret_too_short", length=len(secret_bytes))
                self._enabled = False

    async def dispatch(self, request: Request, call_next):
        # Pass through if disabled
        if not self._enabled:
            return await call_next(request)

        # Skip non-mutating methods
        if request.method not in VERIFIED_METHODS:
            return await call_next(request)

        # Skip exempt routes
        if request.url.path in EXEMPT_ROUTES:
            return await call_next(request)

        # Skip GET-like endpoints that happen to be POST (e.g., pairing)
        if request.url.path.startswith("/api/pairing/"):
            return await call_next(request)

        # Extract security headers
        timestamp_str = request.headers.get("X-Timestamp")
        nonce = request.headers.get("X-Nonce")
        signature = request.headers.get("X-HMAC-Signature")

        if not timestamp_str or not nonce or not signature:
            return JSONResponse(
                status_code=401,
                content={
                    "error": "Missing security headers"
                    " (X-Timestamp, X-Nonce, X-HMAC-Signature)"
                },
            )

        # Parse timestamp
        try:
            timestamp = float(timestamp_str)
        except (ValueError, TypeError):
            return JSONResponse(
                status_code=401,
                content={"error": "Invalid X-Timestamp format"},
            )

        # Check replay (timestamp freshness + nonce uniqueness)
        if not self._detector.check(timestamp, nonce):
            return JSONResponse(
                status_code=403,
                content={"error": "Request rejected (replay detected or timestamp expired)"},
            )

        # Read body for HMAC verification
        body = await request.body()

        # Verify HMAC signature
        if not self._signer.verify(body, timestamp, signature):
            return JSONResponse(
                status_code=401,
                content={"error": "Invalid HMAC signature"},
            )

        # All checks passed
        return await call_next(request)
