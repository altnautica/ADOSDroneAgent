"""MCP server — main service module.

Exposes the drone's full control surface as an MCP server using
the FastMCP framework. Transports:
  - HTTP+SSE on 0.0.0.0:8090/mcp  (LAN, WFB-ng, WiFi AP, USB tether)
  - Unix socket /run/ados/mcp.sock  (loopback, for GCS same-host)
  - stdio via 'ados mcp stdio' CLI wrapper

Architecture:
  FastMCP instance → Tools + Resources + Prompts (all stubs in Phase 1)
  → Gate layer (Phase 2, currently a passthrough)
  → Shim layer (Phase 2, calls existing :8080 REST and IPC sockets)

The service also manages:
  - TokenStore for session token CRUD
  - AuditLog for recording all operations
  - McpMdns for mDNS _ados-mcp._tcp advertisement
  - REST endpoints under /mcp/* mounted on the FastAPI app (Phase 1)

Pairing API (Phase 1, HTTP, no auth required):
  POST /mcp/pair          → {token_id, mnemonic, expires_at}
  GET  /mcp/tokens        → [token list] (auth required)
  POST /mcp/tokens/{id}/revoke → 204 (auth required)
  GET  /mcp/audit/tail    → [last N entries] (auth required)
  GET  /mcp/status        → service health
  POST /mcp/operator-present  → {present: bool}

The MCP protocol endpoint is at /mcp.
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import socket
import time
from pathlib import Path

import structlog
import uvicorn
from mcp.server.fastmcp import FastMCP
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Mount, Route

from ados.core.config import load_config
from ados.core.logging import configure_logging
from ados import __version__

from .audit import AuditLog
from .mdns import McpMdns
from .tokens import TokenStore
from .tools import register_all as register_all_tools

log = structlog.get_logger()

# Global operator-present state (updated via /mcp/operator-present)
_operator_present: bool = False
_operator_present_since: float | None = None
_OPERATOR_PRESENT_TIMEOUT = 10.0  # seconds


def _sd_notify(message: bytes) -> None:
    addr = os.environ.get("NOTIFY_SOCKET", "/run/systemd/notify")
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        sock.sendto(message, addr)
        sock.close()
    except OSError:
        pass


def build_app(
    token_store: TokenStore,
    audit_log: AuditLog,
) -> Starlette:
    """Build the combined Starlette app: MCP protocol + pairing REST API."""

    # FastMCP server
    mcp = FastMCP(
        name="ADOS Drone Agent",
        instructions=(
            "MCP server for ADOS Drone Agent. "
            "Tools control the drone. "
            "Resources stream telemetry and sensor data. "
            "Prompts generate structured flight briefs."
        ),
    )

    # Register all tool stubs (Phase 1 — returns not_implemented)
    register_all_tools(mcp)

    # Pairing and management REST endpoints
    async def pair(request: Request) -> JSONResponse:
        """POST /mcp/pair — mint a new session token."""
        body: dict = {}
        try:
            body = await request.json()
        except Exception:
            pass

        client_hint = body.get("client_hint", request.headers.get("User-Agent", "unknown"))[:64]
        token, mnemonic = token_store.mint(client_hint=client_hint)
        audit_log.record(
            token_id=token.token_id,
            client_hint=client_hint,
            event="pair",
            target="tokens",
            outcome="SUCCESS",
            latency_ms=0,
        )
        return JSONResponse({
            "token_id": token.token_id,
            "mnemonic": mnemonic,
            "scopes": token.scopes,
            "expires_at": token.expires_at,
            "client_hint": token.client_hint,
        })

    async def tokens_list(request: Request) -> JSONResponse:
        """GET /mcp/tokens — list all tokens (active + revoked)."""
        tokens = token_store.list_all()
        return JSONResponse([
            {
                "token_id": t.token_id,
                "client_hint": t.client_hint,
                "scopes": t.scopes,
                "created_at": t.created_at,
                "expires_at": t.expires_at,
                "revoked": t.revoked,
                "last_used_at": t.last_used_at,
                "active": t.active,
            }
            for t in tokens
        ])

    async def token_revoke(request: Request) -> JSONResponse:
        """POST /mcp/tokens/{token_id}/revoke — revoke a token."""
        token_id = request.path_params["token_id"]
        ok = token_store.revoke(token_id)
        if not ok:
            return JSONResponse({"error": "token not found"}, status_code=404)
        return JSONResponse({"revoked": True, "token_id": token_id})

    async def audit_tail(request: Request) -> JSONResponse:
        """GET /mcp/audit/tail?n=100 — return recent audit entries."""
        try:
            n = int(request.query_params.get("n", "100"))
            n = max(1, min(n, 1000))
        except ValueError:
            n = 100
        entries = audit_log.tail(n)
        return JSONResponse(entries)

    async def status(request: Request) -> JSONResponse:
        """GET /mcp/status — service health."""
        return JSONResponse({
            "status": "healthy",
            "version": __version__,
            "active_tokens": len(token_store.list_active()),
            "operator_present": _operator_present,
        })

    async def operator_present(request: Request) -> JSONResponse:
        """POST /mcp/operator-present — update operator presence signal."""
        global _operator_present, _operator_present_since
        body: dict = {}
        try:
            body = await request.json()
        except Exception:
            pass

        present = bool(body.get("present", False))
        _operator_present = present
        _operator_present_since = time.time() if present else None
        return JSONResponse({
            "operator_present": _operator_present,
            "since": _operator_present_since,
        })

    rest_routes = [
        Route("/pair", endpoint=pair, methods=["POST"]),
        Route("/tokens", endpoint=tokens_list, methods=["GET"]),
        Route("/tokens/{token_id}/revoke", endpoint=token_revoke, methods=["POST"]),
        Route("/audit/tail", endpoint=audit_tail, methods=["GET"]),
        Route("/status", endpoint=status, methods=["GET"]),
        Route("/operator-present", endpoint=operator_present, methods=["POST"]),
    ]

    # Get the FastMCP SSE Starlette app and mount it at /mcp
    mcp_starlette = mcp.sse_app()

    app = Starlette(
        routes=[
            Mount("/mcp", app=mcp_starlette),
            Mount("/mcp-api", routes=rest_routes),
        ]
    )
    return app


class McpService:
    """Manages the lifecycle of the MCP server."""

    def __init__(self) -> None:
        self.config = load_config()
        self.token_store = TokenStore(
            token_dir=self.config.mcp.token_dir,
            default_ttl_days=self.config.mcp.token_default_ttl_days,
        )
        self.audit_log = AuditLog(
            log_dir=self.config.mcp.audit_log_dir,
            rotate_mb=self.config.mcp.audit_rotate_mb,
            read_sample_rate=self.config.mcp.audit_read_sample_rate,
        )
        self.mdns: McpMdns | None = None
        self._server: uvicorn.Server | None = None

    async def run(self) -> None:
        cfg = self.config.mcp
        configure_logging(self.config.logging.level)
        log.info("mcp_service_starting", port=cfg.port)

        # Load tokens from disk
        self.token_store.load_all()

        # Build Starlette app
        app = build_app(self.token_store, self.audit_log)

        # Start mDNS if enabled
        if cfg.mdns_enabled:
            self.mdns = McpMdns(
                port=cfg.port,
                device_id=self.config.agent.device_id,
                agent_version=__version__,
            )
            self.mdns.start()

        # Run uvicorn
        uconfig = uvicorn.Config(
            app=app,
            host=cfg.host,
            port=cfg.port,
            log_level="warning",
            access_log=False,
        )
        self._server = uvicorn.Server(uconfig)

        shutdown = asyncio.Event()
        loop = asyncio.get_event_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, shutdown.set)

        _sd_notify(b"READY=1")
        log.info("mcp_service_ready", port=cfg.port)

        serve_task = asyncio.create_task(self._server.serve())
        await shutdown.wait()

        log.info("mcp_service_shutting_down")
        self._server.should_exit = True
        await serve_task

        if self.mdns:
            self.mdns.stop()
        self.audit_log.close()
        log.info("mcp_service_stopped")
