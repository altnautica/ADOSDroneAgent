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
    assist,
    commands,
    config,
    features,
    fleet,
    ground_station,
    logs,
    foxglove,
    memory,
    models,
    ota,
    rerun as rerun_routes,
    survey,
    pairing,
    params,
    peripherals,
    peripherals_v1,
    ros,
    scripts,
    services,
    signing,
    status,
    suites,
    system,
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
    # MSN-025 Wave B.
    @app.get("/healthz")
    async def health_check():
        return {"status": "ok", "version": __version__}

    # Mount routes
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
    # MSN-028 Phase 4 Track A Wave 3: Peripheral Manager plugin registry.
    # Lives alongside the legacy /api/peripherals hardware scan route.
    app.include_router(peripherals_v1.router, prefix="/api")
    app.include_router(suites.router, prefix="/api")
    app.include_router(fleet.router, prefix="/api")
    app.include_router(features.router, prefix="/api")
    app.include_router(ground_station.router, prefix="/api")
    # ROS 2 environment management (opt-in).
    app.include_router(ros.router, prefix="/api")
    # World Model spatial memory (opt-in, requires ados-memory.service).
    app.include_router(memory.router, prefix="/api")
    # On-drone ML model registry (on-demand install, no bundles).
    app.include_router(models.router, prefix="/api")
    # Survey photogrammetry quality validation and dataset packaging.
    app.include_router(survey.router, prefix="/api")
    # Assist diagnostics and self-heal service.
    app.include_router(assist.router, prefix="/api")
    # Foxglove WebSocket Protocol bridge.
    app.include_router(foxglove.router, prefix="/api")
    # Rerun visualization sink.
    app.include_router(rerun_routes.router, prefix="/api")
    # MAVLink v2 message signing: capability + one-shot FC enrollment.
    # Agent holds no key material; key lives in the GCS browser.
    app.include_router(signing.router, prefix="/api")

    # Ground-station profile: mount the setup webapp at `/` so phones
    # hitting `http://192.168.4.1/` over the captive portal land on
    # `static-ground/index.html` directly. Mount is added AFTER every
    # router above so API routes match first (FastAPI resolves routes
    # in registration order, and the static mount is the catch-all).
    # MSN-025 Wave B, DEC-112.
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
