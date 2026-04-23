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
import contextvars
import json
import os
import signal
import socket
import time
from pathlib import Path
from typing import Any

import structlog
import uvicorn
from mcp.server.fastmcp import FastMCP
from starlette.applications import Starlette
from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Mount, Route

from ados.core.config import load_config
from ados.core.logging import configure_logging
from ados import __version__

from .audit import AuditLog, args_sha256
from .gate import Gate, GateStore, GateResult
from .mdns import McpMdns
from .tokens import TokenStore
from .tools import register_all as register_all_tools

# ContextVar: set by bearer-injection middleware, read by gate wrapper.
_mcp_bearer: contextvars.ContextVar[str | None] = contextvars.ContextVar(
    "mcp_bearer", default=None
)

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


class BearerInjectionMiddleware(BaseHTTPMiddleware):
    """Extracts Authorization: Bearer <secret> and stores in ContextVar."""

    async def dispatch(self, request: Request, call_next):
        auth = request.headers.get("Authorization", "")
        bearer = auth.removeprefix("Bearer ").strip() if auth.startswith("Bearer ") else None
        token_var = _mcp_bearer.set(bearer)
        try:
            return await call_next(request)
        finally:
            _mcp_bearer.reset(token_var)


def _wrap_with_gate(
    mcp: FastMCP,
    token_store: TokenStore,
    audit_log: AuditLog,
    gate: Gate,
    operator_present_getter,
) -> None:
    """Wrap the FastMCP tool manager's call_tool with the gate layer."""
    original_call = mcp._tool_manager.call_tool

    async def gated_call_tool(
        name: str,
        arguments: dict[str, Any],
        context=None,
        convert_result: bool = False,
    ) -> Any:
        bearer = _mcp_bearer.get()
        sim_mode = bool(arguments.get("simulate", False))
        confirm_id = arguments.get("_confirm_id")
        typed_phrase = arguments.get("_typed_phrase")

        start = time.monotonic()
        result = gate.check(
            bearer=bearer,
            tool_name=name,
            confirm_id=confirm_id,
            typed_phrase=typed_phrase,
            sim_mode=sim_mode,
        )

        if not result.passed:
            latency = (time.monotonic() - start) * 1000
            token_id = result.token.token_id if result.token else "anon"
            audit_log.record(
                token_id=token_id,
                client_hint=result.token.client_hint if result.token else "unknown",
                event="gate_block",
                target=name,
                outcome="GATE_BLOCKED",
                latency_ms=latency,
            )
            return {"error": "GATE_BLOCKED", "reason": result.reason}

        token = result.token
        audit_log.record(
            token_id=token.token_id if token else "anon",
            client_hint=token.client_hint if token else "unknown",
            event="tool_call",
            target=name,
            outcome="SUCCESS",
            latency_ms=0,
            args_sha256=args_sha256(arguments) if arguments else None,
        )

        # Remove gate-internal fields before passing to handler
        clean_args = {k: v for k, v in arguments.items()
                      if k not in ("_confirm_id", "_typed_phrase")}

        try:
            output = await original_call(
                name=name,
                arguments=clean_args,
                context=context,
                convert_result=convert_result,
            )
            latency = (time.monotonic() - start) * 1000
            return output
        except Exception as exc:
            latency = (time.monotonic() - start) * 1000
            audit_log.record(
                token_id=token.token_id if token else "anon",
                client_hint=token.client_hint if token else "unknown",
                event="tool_call",
                target=name,
                outcome="ERROR",
                latency_ms=latency,
            )
            raise

    mcp._tool_manager.call_tool = gated_call_tool  # type: ignore[method-assign]


def build_app(
    token_store: TokenStore,
    audit_log: AuditLog,
) -> Starlette:
    """Build the combined Starlette app: MCP protocol + pairing REST API."""

    # Gate and prompt setup
    gate_store = GateStore()
    gate = Gate(
        token_store=token_store,
        gate_store=gate_store,
        operator_present_getter=lambda: _operator_present,
    )

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

    # Register all tool handlers (Phase 1 stubs; Phase 2 real handlers)
    register_all_tools(mcp)

    # Register all prompts
    from .prompts import register_all as register_all_prompts
    register_all_prompts(mcp)

    # Wrap tool execution with the gate layer
    _wrap_with_gate(mcp, token_store, audit_log, gate, lambda: _operator_present)

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

    async def console_invoke(request: Request) -> JSONResponse:
        """POST /mcp-api/invoke — Console terminal dispatcher.

        Body: {
          "type": "tool" | "resource" | "prompt",
          "name": "<tool_name or prompt_name>",
          "uri": "<resource URI>",   // for type=resource
          "args": {}                  // for type=tool
        }

        Used exclusively by the GCS MCP Console terminal. The bearer
        token must be in the Authorization header (same as MCP protocol).
        """
        body: dict = {}
        try:
            body = await request.json()
        except Exception:
            return JSONResponse({"error": "invalid JSON body"}, status_code=400)

        invoke_type = body.get("type")

        if invoke_type == "tool":
            tool_name = body.get("name", "")
            tool_args = body.get("args", {})
            bearer = (request.headers.get("Authorization", "")).removeprefix("Bearer ").strip()
            result = gate.check(bearer=bearer, tool_name=tool_name, sim_mode=bool(tool_args.get("simulate")))
            if not result.passed:
                return JSONResponse({"error": "GATE_BLOCKED", "reason": result.reason}, status_code=403)
            try:
                output = await mcp._tool_manager.call_tool(
                    name=tool_name,
                    arguments=tool_args,
                )
                return JSONResponse({"type": "tool", "name": tool_name, "result": output})
            except Exception as e:
                return JSONResponse({"type": "tool", "name": tool_name, "error": str(e)}, status_code=500)

        elif invoke_type == "prompt":
            prompt_name = body.get("name", "")
            try:
                output = await mcp._prompt_manager.render_prompt(prompt_name, {})
                return JSONResponse({
                    "type": "prompt", "name": prompt_name,
                    "result": [{"role": m.role, "content": m.content.text if hasattr(m.content, 'text') else str(m.content)} for m in (output.messages if hasattr(output, 'messages') else [])],
                })
            except Exception as e:
                return JSONResponse({"type": "prompt", "name": prompt_name, "error": str(e)}, status_code=500)

        elif invoke_type == "resource":
            uri = body.get("uri", "")
            try:
                result = await mcp._resource_manager.read_resource(uri)
                return JSONResponse({"type": "resource", "uri": uri, "result": str(result)})
            except Exception as e:
                return JSONResponse({"type": "resource", "uri": uri, "error": str(e)}, status_code=500)

        return JSONResponse({"error": "unknown invoke type"}, status_code=400)

    async def console_catalog(request: Request) -> JSONResponse:
        """GET /mcp-api/catalog — returns tools, resources, prompts for Console autocomplete."""
        tools = [{"name": t.name, "description": t.description or ""} for t in mcp._tool_manager.list_tools()]
        prompts_list = [{"name": p, "description": ""} for p in (mcp._prompt_manager._prompts.keys() if hasattr(mcp, '_prompt_manager') else [])]
        return JSONResponse({
            "tools": tools,
            "resources": [],
            "prompts": prompts_list,
        })

    rest_routes = [
        Route("/pair", endpoint=pair, methods=["POST"]),
        Route("/tokens", endpoint=tokens_list, methods=["GET"]),
        Route("/tokens/{token_id}/revoke", endpoint=token_revoke, methods=["POST"]),
        Route("/audit/tail", endpoint=audit_tail, methods=["GET"]),
        Route("/status", endpoint=status, methods=["GET"]),
        Route("/operator-present", endpoint=operator_present, methods=["POST"]),
        Route("/invoke", endpoint=console_invoke, methods=["POST"]),
        Route("/catalog", endpoint=console_catalog, methods=["GET"]),
    ]

    # Get the FastMCP SSE Starlette app and mount it at /mcp
    mcp_starlette = mcp.sse_app()

    inner = Starlette(
        routes=[
            Mount("/mcp", app=mcp_starlette),
            Mount("/mcp-api", routes=rest_routes),
        ]
    )

    # Wrap with bearer-injection middleware so gate can read the token
    class _App:
        def __init__(self) -> None:
            self._app = BearerInjectionMiddleware(inner)

        async def __call__(self, scope, receive, send):
            await self._app(scope, receive, send)

    return _App()  # type: ignore[return-value]


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
