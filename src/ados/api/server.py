"""FastAPI REST API server for ADOS Drone Agent."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles

from ados import __version__
from ados.api.deps import set_agent_app
from ados.api.routes import (
    config,
    dashboard,
    display,
    ground_station,
    logs,
    network,
    observability,
    pairing,
    peripherals,
    peripherals_v1,
    plugins,
    setup,
    version,
    video,
    vision_detections,
    vision_models,
    wfb,
    whep,
)
from ados.api.runtime import ensure_api_runtime


def create_app(agent: Any) -> FastAPI:
    """Create and configure the FastAPI application."""
    api_runtime = ensure_api_runtime(agent)
    set_agent_app(api_runtime)

    app = FastAPI(
        title="ADOS Drone Agent",
        version=__version__,
        docs_url="/docs",
    )

    # CORS
    cors_config = api_runtime.config.security.api
    if cors_config.cors_enabled:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=cors_config.effective_cors_origins,
            allow_credentials=True,
            allow_methods=["*"],
            allow_headers=["*"],
        )

    # API-key auth and HMAC/replay protection are enforced by the native control
    # front, which authenticates every route it serves or forwards to this
    # surface. This residual API binds only the internal socket behind the front,
    # so it no longer carries its own auth layers.

    # Rate limiting middleware — added after CORS.
    # Execution order: CORS → Rate Limit → Route handler.
    from ados.security.rate_limit import RateLimitMiddleware
    app.add_middleware(RateLimitMiddleware, rate=10.0, burst=20)

    # Close the logging-store proxy clients on shutdown so the shared
    # connections do not leak across an app teardown. The /api/logs surface and
    # the /api/v2/observability proxy both read the store's query API over its
    # trusted local socket; there is no in-process buffer to install.
    @app.on_event("shutdown")
    async def _close_observability_clients() -> None:
        from ados.api import telemetry_source

        await logs.aclose_clients()
        await observability.aclose_client()
        await telemetry_source.aclose()

    # /healthz is served by the native control front; the residual no longer
    # registers it (the front owns the LAN port and answers the liveness probe).

    # Mount routes
    app.include_router(version.router, prefix="/api")
    app.include_router(config.router, prefix="/api")
    app.include_router(logs.router, prefix="/api")
    # Reverse-proxy bridge to the local logging and telemetry store's query
    # API. Lets a client that can only reach :8080 still read the store, over
    # the store's trusted local socket. Inherits the agent's own auth.
    app.include_router(observability.router, prefix="/api")
    app.include_router(video.router, prefix="/api")
    app.include_router(wfb.router, prefix="/api")
    app.include_router(pairing.router, prefix="/api")
    app.include_router(setup.router, prefix="/api")
    app.include_router(dashboard.router, prefix="/api")
    app.include_router(display.router, prefix="/api")
    app.include_router(peripherals.router, prefix="/api")
    # Peripheral Manager plugin registry. Lives alongside the legacy
    # /api/peripherals hardware scan route.
    app.include_router(peripherals_v1.router, prefix="/api")
    app.include_router(vision_models.router, prefix="/api")
    # Live vision-detection WebSocket bridge. Forwards the engine's
    # detection-batch broadcast socket to the browser as JSON.
    app.include_router(vision_detections.router, prefix="/api")
    app.include_router(ground_station.router, prefix="/api")
    app.include_router(network.router, prefix="/api")
    # Plugin lifecycle: install / enable / disable / remove.
    app.include_router(plugins.router, prefix="/api")

    # The WebSocket-auth ticket mint (POST /api/_ws/ticket) is served by the
    # native control surface; the residual WebSocket routes verify the
    # self-contained HMAC ticket via ados.core.ws_ticket, so there is no Python
    # mint to register here.

    # WHEP reverse-proxy mounted at root (no /api prefix) so WebRTC
    # clients reach the offer/answer exchange at the same host:port as
    # the rest of the agent's REST + WS surface. The proxy forwards to
    # the local MediaMTX WHEP endpoint and is profile-gated to the
    # ground station.
    app.include_router(whep.router)

    from importlib.resources import files

    # On-screen ground-station cockpit. A separate committed bundle from the
    # laptop dashboard, served at /cockpit for the HDMI kiosk (a light SPA, not
    # a Next.js build on the box). Mounted BEFORE the dashboard's ``/`` mount so
    # ``/cockpit/*`` matches here first. The source lives at
    # ADOSDroneAgent/cockpit/; scripts/build-cockpit.sh builds it and copies the
    # output into the ``ados.cockpit.static`` package on the wheel. The cockpit
    # has no client-side URL routing, so a plain html=True static mount is
    # sufficient (``/cockpit`` -> ``/cockpit/`` -> index.html, assets under
    # ``/cockpit/assets/``).
    try:
        import ados.cockpit as _cockpit_pkg
    except ImportError as exc:
        raise RuntimeError(
            "Cockpit package 'ados.cockpit' is missing. "
            "Reinstall the agent package or rebuild from source."
        ) from exc
    cockpit_static_dir = Path(str(files(_cockpit_pkg))) / "static"
    if not cockpit_static_dir.exists():
        raise RuntimeError(
            f"Cockpit static directory missing at {cockpit_static_dir}. "
            "Run scripts/build-cockpit.sh or reinstall the agent package."
        )

    from starlette.responses import RedirectResponse

    async def _cockpit_index_redirect(_request: Any) -> RedirectResponse:
        # A bare /cockpit (no trailing slash) does not match the StaticFiles
        # mount below (which serves /cockpit/), so redirect to it. This lets the
        # kiosk and reach links target the clean /cockpit URL.
        return RedirectResponse(url="/cockpit/")

    app.add_route("/cockpit", _cockpit_index_redirect, include_in_schema=False)
    app.mount(
        "/cockpit",
        StaticFiles(directory=str(cockpit_static_dir), html=True),
        name="cockpit_static",
    )

    # Browser dashboard. Mounted AFTER every router above so API routes
    # match first and `/` serves the SPA entry. The TypeScript source
    # lives at ADOSDroneAgent/dashboard/; CI builds it and copies the
    # output into the ``ados.dashboard.static`` package on the wheel.
    # Resolved via ``importlib.resources`` so editable installs and
    # wheel installs both find the same files.
    #
    # The dashboard is a client-routed SPA (react-router): direct URL
    # loads of paths like /setup or /pairing must resolve to index.html
    # so the router can take over. StaticFiles in html=True mode only
    # serves index.html for directories, not arbitrary missing paths.
    # SpaStaticFiles below adds a 404 → index.html fallback for any
    # request that doesn't map to a real asset and isn't an /api/* path
    # (those are handled earlier in the middleware chain).
    try:
        import ados.dashboard as _dashboard_pkg
    except ImportError as exc:
        raise RuntimeError(
            "Dashboard package 'ados.dashboard' is missing. "
            "Reinstall the agent package or rebuild from source."
        ) from exc
    static_dir = Path(str(files(_dashboard_pkg))) / "static"
    if not static_dir.exists():
        raise RuntimeError(
            f"Dashboard static directory missing at {static_dir}. "
            "Run scripts/build-dashboard.sh or reinstall the agent package."
        )

    from starlette.exceptions import HTTPException as StarletteHTTPException
    from starlette.responses import FileResponse, Response

    class SpaStaticFiles(StaticFiles):
        """StaticFiles + SPA fallback. Unknown paths return index.html
        (200) so the React router can resolve client-side routes; real
        asset 404s still bubble up because they live under /assets/* and
        404 there is a packaging bug, not a missing route.
        """

        index_path: Path

        def __init__(self, *, directory: str, **kwargs: Any) -> None:
            super().__init__(directory=directory, html=True, **kwargs)
            self.index_path = Path(directory) / "index.html"

        async def get_response(self, path: str, scope: dict) -> Response:  # type: ignore[override]
            try:
                return await super().get_response(path, scope)
            except StarletteHTTPException as exc:
                if exc.status_code != 404:
                    raise
                # Don't fall back for asset paths — those should 404 cleanly
                # so the user sees missing files instead of a silent index.
                if path.startswith("assets/") or "." in path.rsplit("/", 1)[-1]:
                    raise
                # Don't fall back for /api/*: an unknown API path means the
                # caller hit a typo'd endpoint or a wrong HTTP method, and
                # returning the SPA HTML there silently masks the real 404 /
                # 405. Plugin and external integrations need crisp errors.
                if path.startswith("api/") or path == "api":
                    raise
                return FileResponse(self.index_path)

    app.mount(
        "/",
        SpaStaticFiles(directory=str(static_dir)),
        name="dashboard_static",
    )

    return app


async def create_api_task(agent: Any) -> None:
    """Create and run the API server as an asyncio task."""
    api_runtime = ensure_api_runtime(agent)
    app = create_app(api_runtime)
    api_config = api_runtime.config.api.rest

    # ADOS_API_INTERNAL_SOCKET redirects this to a single Unix socket behind the
    # native front when it owns the LAN port; otherwise the dual-stack TCP pair.
    from ados.api.dual_bind import make_listen_sockets
    sockets = make_listen_sockets(api_config.host, api_config.port)
    config = uvicorn.Config(
        app,
        log_level="warning",
        access_log=False,
    )
    server = uvicorn.Server(config)
    await server.serve(sockets=sockets)
