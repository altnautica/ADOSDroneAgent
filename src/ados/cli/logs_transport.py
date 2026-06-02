"""Transport for the ``ados logs`` CLI.

Resolves the query-API plane and carries the three call shapes the subcommands
need: a JSON GET (query/aggregate/sessions/stats/openapi), a raw byte stream
(export), and a Server-Sent-Events line stream (tail).

Local-first resolution, on-box wins:

* with no ``--host``, the client talks to the trusted unix query socket. No key
  is sent: the socket is the trusted local plane.
* with ``--host``, the client talks to the LAN TCP port and sends an
  ``X-ADOS-Key`` resolved from ``--key``, the ``ADOS_KEY`` env var, or the
  local pairing file, in that order.

The unix transport uses httpx's ``uds`` support, so the same request/streaming
code path serves both planes.
"""

from __future__ import annotations

import json
import os
from collections.abc import Iterator
from typing import Any

import httpx


class LogsTransportError(Exception):
    """A transport-level failure surfaced as a clean CLI error."""


def _load_pairing_key() -> str | None:
    """Read the agent's pairing key from the local pairing file, if present.

    Mirrors the loader the rest of the CLI uses so the same key works against
    :8090 as against :8080.
    """
    try:
        from ados.core.paths import PAIRING_JSON

        if PAIRING_JSON.exists():
            data = json.loads(PAIRING_JSON.read_text(encoding="utf-8"))
            key = data.get("api_key")
            return key if isinstance(key, str) and key else None
    except (OSError, ValueError, ImportError):
        return None
    return None


class LogsClient:
    """A small query-API client over either the unix socket or the LAN port.

    Use as a context manager so the underlying httpx client (and its socket)
    is always closed::

        with LogsClient(socket_path=..., host=None) as client:
            env = client.get_json("/v1/query", {"limit": 10})
    """

    def __init__(
        self,
        *,
        socket_path: str,
        host: str | None,
        port: int,
        key: str | None,
        timeout: float = 15.0,
    ) -> None:
        self._socket_path = socket_path
        self._host = host
        self._port = port
        self._timeout = timeout
        self._headers: dict[str, str] = {}
        if host:
            # LAN plane: resolve the key (explicit, env, then pairing file) and
            # send it on every request, matching the agent's HTTP auth.
            resolved = key or os.environ.get("ADOS_KEY") or _load_pairing_key()
            if resolved:
                self._headers["X-ADOS-Key"] = resolved
            self._base_url = f"http://{host}:{port}"
            self._transport: httpx.BaseTransport | None = None
        else:
            # On-box plane: the trusted unix socket, no key. The host portion of
            # the URL is a placeholder httpx requires; the uds transport routes
            # to the socket regardless of it.
            self._base_url = "http://logd"
            self._transport = httpx.HTTPTransport(uds=socket_path)
        self._client: httpx.Client | None = None

    def __enter__(self) -> LogsClient:
        self._client = httpx.Client(
            base_url=self._base_url,
            transport=self._transport,
            headers=self._headers,
            timeout=self._timeout,
        )
        return self

    def __exit__(self, *_exc: object) -> None:
        if self._client is not None:
            self._client.close()
            self._client = None

    def _where(self) -> str:
        return f"{self._host}:{self._port}" if self._host else self._socket_path

    def get_json(self, path: str, params: dict[str, Any]) -> dict[str, Any]:
        """GET a path and return the decoded JSON envelope.

        A query-API error body (``{"error": {...}}``) is surfaced as a clean
        CLI error carrying the server's code and message; a transport failure
        explains where it tried to reach.
        """
        assert self._client is not None, "use LogsClient as a context manager"
        try:
            resp = self._client.get(path, params=params)
        except httpx.ConnectError as exc:
            raise LogsTransportError(
                f"could not reach the logging daemon at {self._where()}. "
                "Is ados-logd running? On-box, the default is the unix socket."
            ) from exc
        except httpx.HTTPError as exc:
            raise LogsTransportError(f"request to {self._where()} failed: {exc}") from exc
        return self._decode(resp)

    def stream(self, path: str, params: dict[str, Any]) -> Iterator[bytes]:
        """Stream a path's raw response body in chunks (the export path)."""
        assert self._client is not None, "use LogsClient as a context manager"
        try:
            with self._client.stream("GET", path, params=params) as resp:
                if resp.status_code >= 400:
                    resp.read()
                    self._raise_for_error(resp)
                yield from resp.iter_bytes()
        except httpx.ConnectError as exc:
            raise LogsTransportError(
                f"could not reach the logging daemon at {self._where()}."
            ) from exc
        except httpx.HTTPError as exc:
            raise LogsTransportError(f"stream from {self._where()} failed: {exc}") from exc

    def stream_sse(self, path: str, params: dict[str, Any]) -> Iterator[dict[str, Any]]:
        """Stream a Server-Sent-Events endpoint, yielding each event's decoded
        JSON ``data`` payload (the tail path). Keep-alive comment frames and
        non-JSON data lines are skipped."""
        assert self._client is not None, "use LogsClient as a context manager"
        try:
            with self._client.stream("GET", path, params=params) as resp:
                if resp.status_code >= 400:
                    resp.read()
                    self._raise_for_error(resp)
                for line in resp.iter_lines():
                    # SSE: data lines start with "data:"; comments start with
                    # ":"; blank lines separate events. Only data carries JSON.
                    if not line or not line.startswith("data:"):
                        continue
                    payload = line[len("data:") :].strip()
                    if not payload:
                        continue
                    try:
                        yield json.loads(payload)
                    except json.JSONDecodeError:
                        continue
        except httpx.ConnectError as exc:
            raise LogsTransportError(
                f"could not reach the logging daemon at {self._where()}."
            ) from exc
        except httpx.HTTPError as exc:
            raise LogsTransportError(f"tail from {self._where()} failed: {exc}") from exc

    def _decode(self, resp: httpx.Response) -> dict[str, Any]:
        if resp.status_code >= 400:
            self._raise_for_error(resp)
        try:
            data = resp.json()
        except ValueError as exc:
            raise LogsTransportError(f"response from {self._where()} was not JSON") from exc
        if not isinstance(data, dict):
            return {"data": data}
        return data

    def _raise_for_error(self, resp: httpx.Response) -> None:
        """Raise a clean error from a non-2xx response, preferring the API's
        ``{"error": {code, message}}`` body."""
        detail = f"HTTP {resp.status_code}"
        try:
            body = resp.json()
            err = body.get("error") if isinstance(body, dict) else None
            if isinstance(err, dict):
                detail = f"{err.get('code', 'error')}: {err.get('message', '')}"
        except ValueError:
            text = resp.text.strip()
            if text:
                detail = f"HTTP {resp.status_code}: {text[:160]}"
        raise LogsTransportError(detail)


__all__ = ["LogsClient", "LogsTransportError"]
