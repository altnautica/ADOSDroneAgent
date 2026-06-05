"""HTTP access to the legacy on-box handlers and the durable store.

The store is reached over its on-box unix socket when one is given (no key,
works even when the FastAPI front door is down) and falls back to the LAN TCP
port otherwise. The legacy handlers and the observability proxy share the legacy
base. Every request is bounded by a short timeout so the harness can never hang.

The ``Fetcher`` holds already-built ``httpx.Client`` objects so a test can inject
clients backed by an ``httpx.MockTransport`` and exercise the whole comparison
deterministically without a live service.
"""

from __future__ import annotations

import httpx

# Per-request bound. The harness is a deterministic dry check, never a soak; a
# slow or absent endpoint must surface as a reachability miss, not a hang.
DEFAULT_TIMEOUT_S = 5.0

# A stand-in authority for unix-socket requests: httpx needs a syntactically
# valid http URL even though the socket transport ignores the host.
_UDS_BASE = "http://logd.local"


class Fetcher:
    """Bounded JSON access to the store (direct + observability) and legacy.

    ``logd_clients`` are tried in order until one returns a JSON body, so the
    on-box unix socket can be preferred with the LAN TCP port as the fallback.
    ``legacy_client`` serves both the legacy handlers and the observability proxy
    (they share the legacy base). Any may be ``None`` when that surface is not
    configured for a run.
    """

    def __init__(
        self,
        logd_clients: list[httpx.Client] | None = None,
        legacy_client: httpx.Client | None = None,
        timeout: float = DEFAULT_TIMEOUT_S,
    ) -> None:
        self._logd_clients = logd_clients or []
        self._legacy_client = legacy_client
        self._timeout = timeout

    @classmethod
    def connect(
        cls,
        legacy_base: str | None,
        logd_base: str | None,
        socket: str | None,
        timeout: float = DEFAULT_TIMEOUT_S,
    ) -> Fetcher:
        """Build a fetcher with real transports: a unix-socket store client first
        (when a socket path is given), then a TCP store client, plus the legacy
        client. Construction never performs I/O, so it cannot fail on an absent
        endpoint; misses surface at request time."""
        logd_clients: list[httpx.Client] = []
        if socket:
            transport = httpx.HTTPTransport(uds=socket)
            logd_clients.append(
                httpx.Client(transport=transport, base_url=_UDS_BASE, timeout=timeout)
            )
        if logd_base:
            logd_clients.append(httpx.Client(base_url=logd_base, timeout=timeout))
        legacy_client = (
            httpx.Client(base_url=legacy_base, timeout=timeout) if legacy_base else None
        )
        return cls(logd_clients, legacy_client, timeout)

    def close(self) -> None:
        """Close every client (idempotent)."""
        for client in self._logd_clients:
            client.close()
        if self._legacy_client is not None:
            self._legacy_client.close()

    def __enter__(self) -> Fetcher:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def logd_query(self, params: dict[str, object]) -> list[dict] | None:
        """Query the store's ``/v1/query`` and return its ``data`` row list.

        Tries each store client in order; returns the first JSON ``data`` list,
        or ``None`` when none answered (the store is unreachable on every
        transport). A non-list ``data`` is treated as an empty page.
        """
        for client in self._logd_clients:
            body = _get_json(client, "/v1/query", params)
            if body is None:
                continue
            rows = body.get("data") if isinstance(body, dict) else None
            return rows if isinstance(rows, list) else []
        return None

    def legacy(self, path: str, entries_key: str) -> list[dict] | None:
        """Fetch a legacy handler and return its entry list (under ``entries_key``,
        falling back to ``data``). ``None`` when the legacy surface is absent or
        unreachable."""
        if self._legacy_client is None:
            return None
        body = _get_json(self._legacy_client, path, None)
        if not isinstance(body, dict):
            return None
        rows = body.get(entries_key)
        if not isinstance(rows, list):
            rows = body.get("data")
        return rows if isinstance(rows, list) else []

    def observability(self, path: str, params: dict[str, object]) -> list[dict] | None:
        """Fetch the observability proxy (on the legacy base) and return its
        ``data`` row list. ``None`` when the proxy is not wired or unreachable —
        expected until the proxy route lands."""
        if self._legacy_client is None:
            return None
        body = _get_json(self._legacy_client, path, params)
        if not isinstance(body, dict):
            return None
        rows = body.get("data")
        return rows if isinstance(rows, list) else []


def _get_json(client: httpx.Client, path: str, params: dict[str, object] | None):
    """GET ``path`` and decode JSON, swallowing every transport / decode error
    into ``None`` so one dead endpoint never aborts the run."""
    try:
        resp = client.get(path, params=params)
    except (httpx.HTTPError, OSError):
        return None
    if resp.status_code >= 400:
        return None
    try:
        return resp.json()
    except ValueError:
        return None
