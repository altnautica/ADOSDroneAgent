"""Shim layer — calls existing agent REST API and IPC from MCP tool handlers.

All MCP tool handlers call through this module. No tool handler re-implements
agent logic. The shim calls loopback HTTP to :8080 or reads IPC sockets.

Design:
  - ShimClient wraps httpx.AsyncClient with X-ADOS-Key auth
  - All calls are async
  - 503 from REST = FC_UNREACHABLE or REST_DOWN → raised as ShimError
  - 5s timeout on all calls
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

import httpx
import structlog

log = structlog.get_logger()

REST_BASE = "http://127.0.0.1:8080"
STATE_SOCK = Path(os.environ.get("ADOS_RUN_DIR", "/run/ados")) / "state.sock"
TIMEOUT = 5.0


class ShimError(Exception):
    """Raised when a shim call fails."""
    def __init__(self, message: str, status_code: int = 0) -> None:
        super().__init__(message)
        self.status_code = status_code


def _api_key() -> str:
    """Read the local pairing API key for authenticating to :8080."""
    try:
        path = Path("/etc/ados/pairing.json")
        if path.exists():
            data = json.loads(path.read_text())
            return data.get("api_key", "")
    except Exception:
        pass
    return ""


def _auth_headers() -> dict[str, str]:
    key = _api_key()
    headers = {}
    if key:
        headers["X-ADOS-Key"] = key
    return headers


async def get(path: str, params: dict | None = None) -> Any:
    """GET /api/{path} → parsed JSON or raise ShimError."""
    url = f"{REST_BASE}/api/{path.lstrip('/')}"
    try:
        async with httpx.AsyncClient(timeout=TIMEOUT) as client:
            resp = await client.get(url, params=params, headers=_auth_headers())
        if resp.status_code == 503:
            raise ShimError(f"Service unavailable: {path}", 503)
        resp.raise_for_status()
        return resp.json()
    except ShimError:
        raise
    except httpx.TimeoutException:
        raise ShimError(f"Timeout calling {path}", 408)
    except httpx.HTTPError as e:
        raise ShimError(str(e), getattr(e.response, "status_code", 0) if hasattr(e, "response") else 0)


async def post(path: str, body: dict | None = None) -> Any:
    """POST /api/{path} with JSON body → parsed JSON or raise ShimError."""
    url = f"{REST_BASE}/api/{path.lstrip('/')}"
    try:
        async with httpx.AsyncClient(timeout=TIMEOUT) as client:
            resp = await client.post(url, json=body or {}, headers=_auth_headers())
        if resp.status_code == 503:
            raise ShimError(f"Service unavailable: {path}", 503)
        resp.raise_for_status()
        return resp.json()
    except ShimError:
        raise
    except httpx.TimeoutException:
        raise ShimError(f"Timeout calling {path}", 408)
    except httpx.HTTPError as e:
        raise ShimError(str(e), getattr(e.response, "status_code", 0) if hasattr(e, "response") else 0)


async def put(path: str, body: dict | None = None) -> Any:
    """PUT /api/{path} with JSON body → parsed JSON or raise ShimError."""
    url = f"{REST_BASE}/api/{path.lstrip('/')}"
    try:
        async with httpx.AsyncClient(timeout=TIMEOUT) as client:
            resp = await client.put(url, json=body or {}, headers=_auth_headers())
        if resp.status_code == 503:
            raise ShimError(f"Service unavailable: {path}", 503)
        resp.raise_for_status()
        return resp.json()
    except ShimError:
        raise
    except httpx.TimeoutException:
        raise ShimError(f"Timeout calling {path}", 408)
    except httpx.HTTPError as e:
        raise ShimError(str(e), getattr(e.response, "status_code", 0) if hasattr(e, "response") else 0)


def read_state_sock() -> dict | None:
    """Read the latest state from /run/ados/state.sock (non-blocking)."""
    try:
        import socket as sock_mod
        s = sock_mod.socket(sock_mod.AF_UNIX, sock_mod.SOCK_STREAM)
        s.settimeout(0.5)
        s.connect(str(STATE_SOCK))
        data = b""
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
        s.close()
        return json.loads(data)
    except Exception:
        return None
