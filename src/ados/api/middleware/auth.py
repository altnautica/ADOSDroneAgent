"""API key authentication middleware."""

from __future__ import annotations

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.responses import JSONResponse

from ados.api.deps import get_agent_app

# Routes that don't require authentication
EXEMPT_PATHS = {
    "/",
    "/docs",
    "/openapi.json",
    "/redoc",
    "/api/pairing/info",
    "/api/pairing/code",
    "/api/pairing/claim",
}


def is_exempt(path: str) -> bool:
    """Check if a path is exempt from authentication."""
    return path in EXEMPT_PATHS or path.startswith("/docs")


class ApiKeyAuthMiddleware(BaseHTTPMiddleware):
    """Middleware that enforces API key authentication when the agent is paired."""

    async def dispatch(self, request: Request, call_next):
        # Skip auth for exempt routes
        if is_exempt(request.url.path):
            return await call_next(request)

        # Skip auth for OPTIONS (CORS preflight)
        if request.method == "OPTIONS":
            return await call_next(request)

        app = get_agent_app()
        pm = app.pairing_manager

        # When unpaired, all routes are open (backward compatible)
        if not pm.is_paired:
            return await call_next(request)

        # Check for manually configured API key first (security.api.api_key)
        configured_key = app.config.security.api.api_key
        api_key = request.headers.get("X-ADOS-Key")

        if not api_key:
            return JSONResponse(
                status_code=401,
                content={
                    "detail": "Missing X-ADOS-Key header. "
                    "This agent is paired and requires authentication.",
                },
            )

        # Validate against pairing-generated key, or manually configured key
        if configured_key and api_key == configured_key:
            return await call_next(request)

        if not pm.validate_key(api_key):
            return JSONResponse(
                status_code=401,
                content={"detail": "Invalid API key"},
            )

        return await call_next(request)
