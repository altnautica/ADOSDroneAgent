"""The residual FastAPI app behind the native control front.

The native front (``ados-control``) owns the LAN port, authenticates every
request, and serves every route except a fixed set of permanent prefixes, which
it forwards here over the internal Unix socket (see
``ados.api.internal_socket``). This app mounts only those prefixes:

* ``/api/v1/setup`` — the setup facade
* ``/api/v1/display`` — the LCD/OLED display surface
* ``/api/peripherals`` — the hardware scan
* ``/api/v1/peripherals`` — the peripheral plugin registry
* ``/api/vision`` — model delivery and the detection stream
* ``/whep`` — the WebRTC playback exchange with the local mediamtx

The front never forwards anything else, so a router mounted outside these
prefixes would be unreachable. ``tests/api/test_residual_surface.py`` pins it.
"""

from __future__ import annotations

from typing import Any

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from ados import __version__
from ados.api.deps import set_agent_app
from ados.api.routes import (
    display,
    peripherals,
    peripherals_v1,
    setup,
    vision_detections,
    vision_models,
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
        # No interactive docs, no schema: nothing legitimate browses this app,
        # and the front would never forward those paths anyway.
        docs_url=None,
        redoc_url=None,
        openapi_url=None,
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

    # No auth layer and no rate limiter here: the app listens only on the
    # internal Unix socket, and the front authenticates and charges each
    # caller's budget before it forwards a request.

    app.include_router(setup.router, prefix="/api")
    app.include_router(display.router, prefix="/api")
    app.include_router(peripherals.router, prefix="/api")
    app.include_router(peripherals_v1.router, prefix="/api")
    app.include_router(vision_models.router, prefix="/api")
    # Live vision-detection WebSocket bridge. Forwards the engine's
    # detection-batch broadcast socket to the browser as JSON.
    app.include_router(vision_detections.router, prefix="/api")
    # WHEP reverse-proxy mounted at the root (no /api prefix) so WebRTC clients
    # reach the offer/answer exchange at the same host:port as the rest of the
    # agent's surface. Forwards to the local mediamtx WHEP endpoint on every
    # profile.
    app.include_router(whep.router)

    return app
