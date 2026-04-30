"""FastAPI REST API server for ADOS Drone Agent."""

from __future__ import annotations

from typing import TYPE_CHECKING

import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from ados import __version__
from ados.api.deps import set_agent_app
from ados.api.middleware.auth import ApiKeyAuthMiddleware
from ados.api.routes import (
    commands,
    config,
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
    signing,
    status,
    suites,
    system,
    version,
    video,
    wfb,
)

if TYPE_CHECKING:
    from ados.core.main import AgentApp


def create_app(agent: AgentApp) -> FastAPI:
    """Create and configure the FastAPI application."""
    set_agent_app(agent)

    app = FastAPI(
        title="ADOS Drone Agent",
        version=__version__,
        docs_url="/docs",
    )

    # CORS
    cors_config = agent.config.security.api
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
        enabled=agent.config.security.hmac_enabled,
        secret=agent.config.security.hmac_secret,
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

    # Ground-station profile: mount the setup webapp at `/` so phones
    # hitting `http://192.168.4.1/` over the captive portal land on
    # `static-ground/index.html` directly. Mount is added AFTER every
    # router above so API routes match first (FastAPI resolves routes
    # in registration order, and the static mount is the catch-all).
    if agent.config.agent.profile == "ground_station":
        from pathlib import Path

        from fastapi.staticfiles import StaticFiles

        static_dir = Path(__file__).resolve().parent.parent / "webapp" / "static-ground"
        if static_dir.exists():
            app.mount(
                "/",
                StaticFiles(directory=str(static_dir), html=True),
                name="ground_static",
            )

    return app


async def create_api_task(agent: AgentApp) -> None:
    """Create and run the API server as an asyncio task."""
    app = create_app(agent)
    api_config = agent.config.scripting.rest_api

    config = uvicorn.Config(
        app,
        host=api_config.host,
        port=api_config.port,
        log_level="warning",
        access_log=False,
    )
    server = uvicorn.Server(config)
    await server.serve()
