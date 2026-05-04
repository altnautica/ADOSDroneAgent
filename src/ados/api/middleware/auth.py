"""API key authentication middleware."""

from __future__ import annotations

from urllib.parse import urlparse

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
    "/api/v1/setup/status",
}

# Setup mutation endpoints that follow the same-origin trust model.
# Reachable without an API key when the request originates from a browser
# served the agent's own static webapp (same-origin). Treated as a
# physical-presence-on-the-LAN gate, not full authentication. The
# `security.setup_token_required` flag escalates to a token requirement.
SAME_ORIGIN_SETUP_PATHS = {
    "/api/v1/setup/remote-access/cloudflare",
    "/api/v1/setup/cloud-choice",
}

# Hostnames the agent itself binds. A request whose Origin header matches
# one of these is considered same-origin. Augmented at runtime by the
# discovered listener IPs in setup/service.py.
LOCAL_HOST_DEFAULTS = {"localhost", "127.0.0.1", "192.168.4.1", "192.168.7.1"}


def is_exempt(path: str) -> bool:
    """Check if a path is exempt from authentication."""
    if path in EXEMPT_PATHS or path.startswith("/docs"):
        return True
    # Static setup assets are served from `/` after all API routes. They
    # must remain readable on first boot and after pairing so users can
    # reopen onboarding from a captive portal or local URL.
    return not path.startswith("/api/")


def _origin_host(request: Request) -> str | None:
    origin = request.headers.get("origin") or request.headers.get("referer")
    if not origin:
        return None
    try:
        return urlparse(origin).hostname
    except Exception:
        return None


def _is_same_origin(request: Request) -> bool:
    """True when the request's Origin/Referer points at this agent."""
    host = _origin_host(request)
    if not host:
        # No Origin / Referer header. Browsers send Origin on cross-origin
        # POSTs and on most fetches; absence is consistent with a server-
        # to-server caller, which we do NOT consider same-origin.
        return False
    if host in LOCAL_HOST_DEFAULTS:
        return True
    # Compare against the request's own Host header so reverse-proxied
    # mDNS / LAN IP / hotspot addresses are accepted without an explicit
    # whitelist update.
    request_host = (request.headers.get("host") or "").split(":", 1)[0]
    return bool(request_host) and host == request_host


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

        # Same-origin trust path for setup mutations. The default posture
        # accepts a same-origin browser without an API key. The
        # ``security.setup_token_required`` knob escalates to requiring
        # ``X-ADOS-Setup-Token`` instead.
        if request.url.path in SAME_ORIGIN_SETUP_PATHS:
            require_token = bool(
                getattr(app.config.security, "setup_token_required", False)
            )
            if not require_token and _is_same_origin(request):
                return await call_next(request)
            if require_token:
                provided = request.headers.get("X-ADOS-Setup-Token")
                if provided:
                    expected = _load_setup_token()
                    if expected and provided == expected:
                        return await call_next(request)
                return JSONResponse(
                    status_code=401,
                    content={
                        "detail": "Missing or invalid X-ADOS-Setup-Token header. "
                        "Setup token is printed by the local CLI.",
                    },
                )

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


def _load_setup_token() -> str | None:
    """Load the same-origin setup token from disk.

    Lazy import keeps the middleware free of filesystem cost on the
    common (token-not-required) path.
    """
    from ados.core.paths import SETUP_TOKEN_PATH

    try:
        if SETUP_TOKEN_PATH.is_file():
            return SETUP_TOKEN_PATH.read_text(encoding="utf-8").strip() or None
    except OSError:
        pass
    return None
