"""Tests for the observability reverse-proxy and the logd-sourced /api/logs.

The proxy at /api/v2/observability/* forwards to the local logging and
telemetry store's query API over a Unix socket; the legacy /api/logs and
/api/logs/stream read the same store and re-map its rows to the legacy shape.
Tests use ``httpx.MockTransport`` to stand in for the store so the suite never
needs a real daemon or socket.
"""

from __future__ import annotations

import json
from collections.abc import Callable
from typing import Any

import httpx
import pytest
from fastapi.testclient import TestClient

from ados.api.routes import logs as logs_module
from ados.api.routes import observability as observability_module
from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime

UpstreamHandler = Callable[[httpx.Request], httpx.Response]


def _build_runtime(profile: str = "auto", *, paired: bool = False) -> Any:
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    runtime = build_api_runtime(config=cfg)
    runtime.pairing_manager.is_paired = paired
    return runtime


@pytest.fixture
def client() -> TestClient:
    return TestClient(create_app(_build_runtime()))


@pytest.fixture
def install_store(monkeypatch: pytest.MonkeyPatch):
    """Swap every store-facing httpx client (proxy + logs query + logs tail)
    for one routed through a MockTransport-backed handler. Returns a factory
    that, given a handler, installs it everywhere and captures the requests."""

    def _install(handler: UpstreamHandler) -> list[httpx.Request]:
        captured: list[httpx.Request] = []

        def _wrapper(request: httpx.Request) -> httpx.Response:
            captured.append(request)
            return handler(request)

        transport = httpx.MockTransport(_wrapper)
        proxy_client = httpx.AsyncClient(
            base_url=observability_module._UPSTREAM_BASE, transport=transport
        )
        query_client = httpx.AsyncClient(
            base_url=logs_module._UPSTREAM_BASE, transport=transport
        )
        tail_client = httpx.AsyncClient(
            base_url=logs_module._UPSTREAM_BASE, transport=transport
        )
        monkeypatch.setattr(observability_module, "_client_singleton", proxy_client)
        monkeypatch.setattr(logs_module, "_query_client", query_client)
        monkeypatch.setattr(logs_module, "_tail_client", tail_client)
        return captured

    return _install


# ---------------------------------------------------------------------------
# Proxy: path + query-string forwarding
# ---------------------------------------------------------------------------


def test_proxy_forwards_path_and_query_string(client, install_store) -> None:
    """/api/v2/observability/v1/query?limit=10 reaches the store at
    /v1/query?limit=10 with the body and status passed through."""
    envelope = {
        "data": [{"id": 1, "ts_us": 1, "source": "api", "level": "info", "msg": "x"}],
        "page": {"next_cursor": None, "count": 1},
        "meta": {"source": "logd", "v": 1, "ts": "2026-06-02T00:00:00+05:30"},
    }

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/query"
        assert request.url.params.get("limit") == "10"
        return httpx.Response(
            200, headers={"content-type": "application/json"}, json=envelope
        )

    captured = install_store(_handler)
    resp = client.get("/api/v2/observability/v1/query?limit=10")
    assert resp.status_code == 200
    assert resp.json() == envelope
    assert resp.headers["content-type"] == "application/json"
    assert len(captured) == 1


def test_proxy_drops_caller_api_key_to_the_socket(client, install_store) -> None:
    """The trusted socket plane carries no key; the X-ADOS-Key the caller sent
    to :8080 must not be forwarded onward to the store."""

    def _handler(request: httpx.Request) -> httpx.Response:
        assert "x-ados-key" not in {k.lower() for k in request.headers}
        return httpx.Response(200, json={"data": []})

    install_store(_handler)
    resp = client.get(
        "/api/v2/observability/v1/query", headers={"X-ADOS-Key": "secret"}
    )
    assert resp.status_code == 200


def test_proxy_passes_through_error_envelope_and_status(client, install_store) -> None:
    """A 400 + the store's error envelope passes through unchanged."""
    err = {"error": {"code": "bad_cursor", "message": "cursor does not parse"}}

    def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(400, json=err)

    install_store(_handler)
    resp = client.get("/api/v2/observability/v1/query?cursor=bad")
    assert resp.status_code == 400
    assert resp.json() == err


def test_proxy_returns_503_when_store_unreachable(client, monkeypatch) -> None:
    """Socket missing / connection refused yields a clean 503 with the standard
    error envelope so the client cascades to its next tier."""

    def _refuse(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("connection refused", request=request)

    transport = httpx.MockTransport(_refuse)
    monkeypatch.setattr(
        observability_module,
        "_client_singleton",
        httpx.AsyncClient(
            base_url=observability_module._UPSTREAM_BASE, transport=transport
        ),
    )
    resp = client.get("/api/v2/observability/v1/query")
    assert resp.status_code == 503
    body = resp.json()
    assert body["error"]["code"] == "service_unavailable"


# ---------------------------------------------------------------------------
# Proxy: streaming preserved (SSE tail + chunked export)
# ---------------------------------------------------------------------------


def test_proxy_streams_sse_tail(client, install_store) -> None:
    """/v1/tail is forwarded with the text/event-stream content type and the
    SSE frames flow through."""
    sse_body = (
        ': keep-alive\n\n'
        'data: {"id":1,"ts_us":1,"level":"info","msg":"a"}\n\n'
        'data: {"id":2,"ts_us":2,"level":"warn","msg":"b"}\n\n'
    )

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/tail"
        return httpx.Response(
            200, headers={"content-type": "text/event-stream"}, content=sse_body
        )

    install_store(_handler)
    resp = client.get("/api/v2/observability/v1/tail")
    assert resp.status_code == 200
    assert resp.headers["content-type"].startswith("text/event-stream")
    assert resp.text == sse_body


def test_proxy_streams_export_bytes(client, install_store) -> None:
    """/v1/export is forwarded as a jsonl byte stream without translation."""
    jsonl = b'{"id":1}\n{"id":2}\n{"id":3}\n'

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/export"
        return httpx.Response(
            200, headers={"content-type": "application/jsonl"}, content=jsonl
        )

    install_store(_handler)
    resp = client.get("/api/v2/observability/v1/export?format=jsonl")
    assert resp.status_code == 200
    assert resp.content == jsonl
    assert resp.headers["content-type"] == "application/jsonl"


# ---------------------------------------------------------------------------
# Legacy /api/logs sourced from the store
# ---------------------------------------------------------------------------


def test_legacy_logs_maps_store_rows_to_legacy_shape(client, install_store) -> None:
    """GET /api/logs reads the store and maps each row to the legacy
    { seq, timestamp, level, logger, message } entry."""
    rows = [
        {
            "id": 2,
            "ts_us": 1_700_000_000_000_000,
            "source": "python-agent",
            "level": "warn",
            "target": "ados.api",
            "msg": "second",
        },
        {
            "id": 1,
            "ts_us": 1_699_999_000_000_000,
            "source": "python-agent",
            "level": "info",
            "target": "ados.core",
            "msg": "first",
        },
    ]

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/query"
        assert request.url.params.get("kind") == "logs"
        return httpx.Response(200, json={"data": rows, "page": {}, "meta": {}})

    install_store(_handler)
    resp = client.get("/api/logs?limit=10")
    assert resp.status_code == 200
    data = resp.json()
    assert data["total"] == 2
    first = data["entries"][0]
    assert set(first) == {"seq", "timestamp", "level", "logger", "message"}
    assert first["level"] == "WARN"
    assert first["logger"] == "ados.api"
    assert first["message"] == "second"
    assert isinstance(first["timestamp"], str) and "T" in first["timestamp"]


def test_legacy_logs_level_filter_lowercased_to_store(client, install_store) -> None:
    """The legacy upper-case level filter is lower-cased for the store and
    re-applied for parity."""

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.params.get("level") == "error"
        return httpx.Response(
            200,
            json={
                "data": [
                    {"id": 1, "ts_us": 1, "source": "s", "level": "error", "msg": "e"}
                ]
            },
        )

    install_store(_handler)
    resp = client.get("/api/logs?level=ERROR")
    assert resp.status_code == 200
    assert resp.json()["entries"][0]["level"] == "ERROR"


def test_legacy_logs_offset_limit_paging(client, install_store) -> None:
    """The legacy offset/limit window is honored over the store rows."""
    rows = [
        {"id": i, "ts_us": i, "source": "s", "level": "info", "msg": f"m{i}"}
        for i in range(10)
    ]

    def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={"data": rows})

    install_store(_handler)
    resp = client.get("/api/logs?limit=3&offset=2")
    assert resp.status_code == 200
    entries = resp.json()["entries"]
    assert len(entries) == 3
    assert [e["message"] for e in entries] == ["m2", "m3", "m4"]


def test_legacy_logs_stream_remaps_tail_to_legacy_frames(client, install_store) -> None:
    """GET /api/logs/stream proxies the store tail and re-maps each row to the
    legacy data frame; keep-alive comments pass through."""
    sse_body = (
        ': keep-alive\n\n'
        'data: {"id":1,"ts_us":1700000000000000,"source":"s","level":"info","target":"ados.api","msg":"hi"}\n\n'
        'data: {"kind":"lagged","dropped":5}\n\n'
    )

    def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/tail"
        assert request.url.params.get("replay") == "100"
        return httpx.Response(
            200, headers={"content-type": "text/event-stream"}, content=sse_body
        )

    install_store(_handler)
    resp = client.get("/api/logs/stream")
    assert resp.status_code == 200
    assert resp.headers["content-type"].startswith("text/event-stream")
    # The data frame is re-mapped to the legacy shape.
    data_lines = [
        ln for ln in resp.text.splitlines() if ln.startswith("data:")
    ]
    assert len(data_lines) == 1
    payload = json.loads(data_lines[0][len("data:") :].strip())
    assert payload["level"] == "INFO"
    assert payload["logger"] == "ados.api"
    assert payload["message"] == "hi"
    # The keep-alive and the lagged-notice both render as comments, not data.
    assert ": keep-alive" in resp.text


def test_legacy_logs_stream_closes_clean_when_store_down(client, monkeypatch) -> None:
    """When the store is unreachable the legacy stream emits a comment and
    closes rather than 500ing."""

    def _refuse(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("refused", request=request)

    transport = httpx.MockTransport(_refuse)
    monkeypatch.setattr(
        logs_module,
        "_tail_client",
        httpx.AsyncClient(base_url=logs_module._UPSTREAM_BASE, transport=transport),
    )
    resp = client.get("/api/logs/stream")
    assert resp.status_code == 200
    assert "logging store unavailable" in resp.text


# ---------------------------------------------------------------------------
# Auth parity: the proxy inherits the agent's own auth, no second layer
# ---------------------------------------------------------------------------


def test_proxy_is_open_when_unpaired(install_store) -> None:
    """Unpaired: being on the LAN is the auth boundary, so the proxy answers
    with no key (matching the pairing-claim posture)."""
    client = TestClient(create_app(_build_runtime(paired=False)))

    def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, json={"data": []})

    install_store(_handler)
    resp = client.get("/api/v2/observability/v1/query")
    assert resp.status_code == 200
