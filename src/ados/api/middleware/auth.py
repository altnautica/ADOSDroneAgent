"""API key authentication middleware."""

from __future__ import annotations

import os
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

# Cosmetic setup-wizard mutations that follow the same-origin trust model.
# Reachable without an API key when the request originates from a browser
# served the agent's own static webapp (same-origin). Treated as a
# physical-presence-on-the-LAN gate, not full authentication. These only move
# the wizard's progress state; they do not change cloud posture or write a
# credential. The `security.setup_token_required` flag escalates to a token
# requirement.
SAME_ORIGIN_SETUP_PATHS = {
    "/api/v1/setup/finish",
    "/api/v1/setup/skip",
    "/api/v1/setup/reset",
}

# Cloud-posture-mutating setup routes. These are NOT covered by the bare
# same-origin gate: cloud-choice flips the agent's cloud posture and
# remote-access/cloudflare writes a root-owned tunnel token, so a forged-Origin
# LAN caller must NOT reach them with no credential. On a paired agent they
# require the API key (handled by the general paired-route path below); the
# setup-token escalation still applies on an unpaired agent. The unpaired-
# default is the same key-or-token gate, never bare same-origin.
SAME_ORIGIN_SETUP_CLOUD_PATHS = {
    "/api/v1/setup/remote-access/cloudflare",
    "/api/v1/setup/cloud-choice",
}

# Path prefixes that follow the same same-origin trust model. Used for
# routes with a path parameter where exact-match would not work.
SAME_ORIGIN_SETUP_PREFIXES = (
    "/api/v1/setup/step/",
    "/api/v1/setup/nudges",
)

# Hostnames the agent itself binds. A request whose Origin header matches
# one of these is considered same-origin. Augmented at runtime by the
# discovered listener IPs in setup/service.py.
LOCAL_HOST_DEFAULTS = {"localhost", "127.0.0.1", "192.168.4.1", "192.168.7.1"}

# Loopback peer addresses that identify an on-box caller (the local CLI).
_LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost"}

# Proxy / tunnel relay headers. Their presence means the request was forwarded
# by a reverse proxy or tunnel (e.g. a Cloudflare Tunnel terminating on
# 127.0.0.1) rather than originating on this host, so it must NOT qualify for
# on-box loopback trust.
_FORWARDED_HEADERS = ("x-forwarded-for", "x-real-ip", "forwarded", "cf-connecting-ip")

# The trustworthy on-box signal the native front stamps when this API runs
# behind it (bound to the internal Unix socket). The front strips any
# client-supplied copy and sets it only on a genuine loopback request, so it is
# authoritative — but ONLY behind the front (see `_behind_front`).
_ONBOX_HEADER = "x-ados-onbox"


def _behind_front() -> bool:
    """True when this API is served behind the native front (bound to the
    internal Unix socket rather than the LAN TCP port).

    Signalled by ``ADOS_API_INTERNAL_SOCKET`` being set, which is exactly the
    env the front unit injects at cutover. Read live (not cached) so a test can
    toggle it; the unit restarts on a real flip, so the cost is irrelevant.
    """
    return bool(os.environ.get("ADOS_API_INTERNAL_SOCKET", "").strip())


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


def _is_on_box(request: Request) -> bool:
    """True when the request originates on this host's loopback interface and
    was not relayed by a proxy or tunnel.

    An on-box caller (the local ``ados`` CLI, a root-owned job) already holds
    shell-level privilege on the machine, which strictly exceeds API
    authentication. Trusting loopback lets the on-box CLI reach authenticated
    routes (``ados radio status`` and friends) on a *paired* agent without
    reading the root-owned pairing key file (``/etc/ados/pairing.json`` is
    ``0600 root``, so a non-root CLI process cannot load the key and would
    otherwise 401). A proxy or tunnel that terminates on 127.0.0.1 is excluded
    by the forwarding-header check, so it can never impersonate an on-box
    caller to bypass authentication.

    The native control surface mirrors this exact contract: the peer socket
    address is loopback AND no proxy-forwarding header is present.

    When this API is served BEHIND the native front (bound to an internal Unix
    socket, signalled by ``ADOS_API_INTERNAL_SOCKET``), the real TCP peer is the
    socket, so ``request.client`` is unreliable. The front then conveys on-box
    trust explicitly: it STRIPS any client-supplied ``X-ADOS-Onbox`` and sets it
    to ``1`` only when its own loopback check passes, so the header is
    authoritative in that mode. The header is trusted ONLY behind the front —
    when FastAPI owns the TCP port directly an off-box client could spoof it, so
    it is ignored unless the internal-socket env is set.
    """
    if _behind_front():
        return request.headers.get(_ONBOX_HEADER) == "1"
    client = request.client
    if client is None or client.host not in _LOOPBACK_HOSTS:
        return False
    return not any(h in request.headers for h in _FORWARDED_HEADERS)


class ApiKeyAuthMiddleware(BaseHTTPMiddleware):
    """Middleware that enforces API key authentication when the agent is paired."""

    async def dispatch(self, request: Request, call_next):
        # Skip auth for exempt routes
        if is_exempt(request.url.path):
            return await call_next(request)

        # Skip auth for OPTIONS (CORS preflight)
        if request.method == "OPTIONS":
            return await call_next(request)

        # On-box loopback trust: a request from this host's own loopback
        # interface is the local operator, who already holds shell-level
        # privilege that exceeds API auth. This lets the on-box CLI work on a
        # paired agent without the root-owned pairing key. Proxy-forwarded
        # requests are excluded so a tunnel terminating on 127.0.0.1 cannot
        # bypass authentication.
        if _is_on_box(request):
            return await call_next(request)

        app = get_agent_app()
        pm = app.pairing_manager

        # Cloud-posture-mutating setup routes (cloud-choice, the cloudflare
        # tunnel-token write). A bare same-origin browser is NOT enough here:
        # an attacker can forge Origin/Referer/Host to match the agent, so these
        # impactful routes always require a real credential. A valid API key
        # always passes; otherwise a valid setup token passes; otherwise 401.
        # This gate runs BEFORE the unpaired open-pass so a fresh agent cannot
        # have its cloud posture flipped by a forged-Origin LAN caller.
        if request.url.path in SAME_ORIGIN_SETUP_CLOUD_PATHS:
            if _valid_api_key(app, pm, request):
                return await call_next(request)
            if _valid_setup_token(request):
                return await call_next(request)
            return JSONResponse(
                status_code=401,
                content={
                    "detail": "This setup route changes cloud posture and "
                    "requires the API key (X-ADOS-Key) or the setup token "
                    "(X-ADOS-Setup-Token, printed by `ados status`).",
                },
            )

        # Same-origin trust path for the cosmetic setup-wizard mutations. The
        # default posture accepts a same-origin browser without an API key. The
        # ``security.setup_token_required`` knob escalates to requiring
        # ``X-ADOS-Setup-Token`` instead.
        is_setup_mutation = request.url.path in SAME_ORIGIN_SETUP_PATHS or any(
            request.url.path.startswith(prefix) for prefix in SAME_ORIGIN_SETUP_PREFIXES
        )
        if is_setup_mutation:
            require_token = bool(
                getattr(app.config.security, "setup_token_required", False)
            )
            if not require_token and _is_same_origin(request):
                return await call_next(request)
            if require_token:
                if _valid_setup_token(request):
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

        # Same-origin trust is scoped to the setup-mutation surface only (handled
        # above). It is NOT extended to general paired routes: an attacker can set
        # any Origin/Referer to match the agent's own Host, so a blanket
        # same-origin bypass would let a forged header reach an authenticated
        # route without the key. The dashboard the agent serves carries
        # ``X-ADOS-Key`` (from localStorage after pairing) on every authenticated
        # fetch, so it does not rely on a header-only bypass. Paired non-setup
        # routes therefore require the key unconditionally below.

        api_key = request.headers.get("X-ADOS-Key")

        if not api_key:
            # No key on a direct LAN visit to the served dashboard: tell the
            # operator exactly where to get it (`ados status` prints the key)
            # so a direct visit shows an actionable message, not a blank/hung
            # UI. The same-origin shortcut is deliberately NOT reintroduced for
            # general routes.
            return JSONResponse(
                status_code=401,
                content={
                    "detail": "Missing X-ADOS-Key header. This agent is paired "
                    "and requires authentication. Run `ados status` on the "
                    "agent to print the API key, then enter it in the "
                    "dashboard.",
                },
            )

        if not _valid_api_key(app, pm, request):
            return JSONResponse(
                status_code=401,
                content={"detail": "Invalid API key"},
            )

        return await call_next(request)


def _valid_api_key(app, pm, request: Request) -> bool:
    """True when the request carries a valid API key.

    Matches the manually-configured ``security.api.api_key`` or a
    pairing-generated key. A missing header is not valid. Shared by the
    cloud-posture gate and the general paired-route check so both validate the
    key identically.
    """
    api_key = request.headers.get("X-ADOS-Key")
    if not api_key:
        return False
    configured_key = app.config.security.api.api_key
    if configured_key and api_key == configured_key:
        return True
    return bool(pm.validate_key(api_key))


def _valid_setup_token(request: Request) -> bool:
    """True when the request carries the valid ``X-ADOS-Setup-Token``."""
    provided = request.headers.get("X-ADOS-Setup-Token")
    if not provided:
        return False
    expected = _load_setup_token()
    return bool(expected and provided == expected)


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
