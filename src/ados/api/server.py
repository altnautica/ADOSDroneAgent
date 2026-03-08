"""FastAPI REST API server for ADOS Drone Agent."""

from __future__ import annotations

from typing import TYPE_CHECKING

import uvicorn
from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from ados import __version__
from ados.api.deps import set_agent_app
from ados.api.routes import commands, config, logs, params, services, status, video, wfb

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

    # Health check
    @app.get("/")
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
