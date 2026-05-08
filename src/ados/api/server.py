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
from ados.api.middleware.auth import ApiKeyAuthMiddleware
from ados.api.routes import (
    commands,
    config,
    dashboard,
    diagnostics,
    display,
    features,
    fleet,
    ground_station,
    logs,
    ota,
    pairing,
    params,
    peripherals,
    peripherals_v1,
    plugins,
    ros,
    scripts,
    services,
    setup,
    signing,
    status,
    suites,
    system,
    version,
    video,
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
            allow_origins=cors_config.cors_origins,
            allow_credentials=True,
            allow_methods=["*"],
            allow_headers=["*"],
        )

    # Auth middleware — added after CORS.
    # FastAPI/Starlette executes middleware in reverse order of add_middleware() calls,
    # so auth runs AFTER CORS headers are added but BEFORE rate limiting.
    app.add_middleware(ApiKeyAuthMiddleware)

    # HMAC + replay protection — added after auth, before rate limit.
    # Only active when config.security.hmac_enabled = True.
    from ados.api.middleware.security import SecurityMiddleware
    app.add_middleware(
        SecurityMiddleware,
        enabled=api_runtime.config.security.hmac_enabled,
        secret=api_runtime.config.security.hmac_secret,
    )

    # Rate limiting middleware — added after auth + security.
    # Execution order: CORS → Auth → Security (HMAC+Replay) → Rate Limit → Route handler.
    from ados.security.rate_limit import RateLimitMiddleware
    app.add_middleware(RateLimitMiddleware, rate=10.0, burst=20)

    # Health check. Moved off `/` so the ground-station static mount
    # can own the root path (`/` -> `static-ground/index.html`).
    @app.get("/healthz")
    async def health_check():
        return {"status": "ok", "version": __version__}

    # Mount routes
    app.include_router(version.router, prefix="/api")
    app.include_router(status.router, prefix="/api")
    app.include_router(services.router, prefix="/api")
    app.include_router(params.router, prefix="/api")
    app.include_router(commands.router, prefix="/api")
    app.include_router(config.router, prefix="/api")
    app.include_router(logs.router, prefix="/api")
    app.include_router(video.router, prefix="/api")
    app.include_router(wfb.router, prefix="/api")
    app.include_router(scripts.router, prefix="/api")
    app.include_router(ota.router, prefix="/api")
    app.include_router(pairing.router, prefix="/api")
    app.include_router(system.router, prefix="/api")
    app.include_router(setup.router, prefix="/api")
    app.include_router(dashboard.router, prefix="/api")
    app.include_router(diagnostics.router, prefix="/api")
    app.include_router(display.router, prefix="/api")
    app.include_router(peripherals.router, prefix="/api")
    # Peripheral Manager plugin registry. Lives alongside the legacy
    # /api/peripherals hardware scan route.
    app.include_router(peripherals_v1.router, prefix="/api")
    app.include_router(suites.router, prefix="/api")
    app.include_router(fleet.router, prefix="/api")
    app.include_router(features.router, prefix="/api")
    app.include_router(ground_station.router, prefix="/api")
    # ROS 2 environment management (opt-in).
    app.include_router(ros.router, prefix="/api")
    # MAVLink v2 message signing: capability + one-shot FC enrollment.
    # Agent holds no key material; key lives in the GCS browser.
    app.include_router(signing.router, prefix="/api")
    # Plugin lifecycle: install / enable / disable / remove.
    app.include_router(plugins.router, prefix="/api")

    # WHEP reverse-proxy mounted at root (no /api prefix) so WebRTC
    # clients reach the offer/answer exchange at the same host:port as
    # the rest of the agent's REST + WS surface. The proxy forwards to
    # the local MediaMTX WHEP endpoint and is profile-gated to the
    # ground station.
    app.include_router(whep.router)

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
    from importlib.resources import files
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
    api_config = api_runtime.config.scripting.rest_api

    config = uvicorn.Config(
        app,
        host=api_config.host,
        port=api_config.port,
        log_level="warning",
        access_log=False,
    )
    server = uvicorn.Server(config)
    await server.serve()
