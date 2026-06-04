"""Tests for the ``ados logs`` CLI group and its transport resolution."""

from __future__ import annotations

import json
import os
from unittest.mock import patch

from click.testing import CliRunner

import ados.cli.logs as logs_mod
from ados.cli.logs import logs_group
from ados.cli.logs_transport import LogsClient

runner = CliRunner()


def _query_envelope() -> dict:
    return {
        "data": [
            {
                "id": 2,
                "ts_us": 1_700_000_000_500_000,
                "session": 1,
                "source": "ados-video",
                "level": "warn",
                "target": "video::encode",
                "msg": "encoder stalled",
                "fields": {"attempt": 3},
                "redacted": False,
            },
            {
                "id": 1,
                "ts_us": 1_700_000_000_000_000,
                "session": 1,
                "source": "api",
                "level": "info",
                "target": None,
                "msg": "started",
                "fields": {},
                "redacted": False,
            },
        ],
        "page": {"next_cursor": "abc123", "count": 2},
        "meta": {"source": "logd", "v": 1, "ts": 1_700_000_001_000_000, "db_lag_ms": 5},
    }


def _stats_envelope() -> dict:
    return {
        "data": {
            "db_size_bytes": 40960,
            "wal_size_bytes": 0,
            "schema_version": 2,
            "integrity": "ok",
            "rows": {"logs": 12, "metrics": 4, "events": 1, "hw": 2},
            "oldest_ts_us": 1_700_000_000_000_000,
            "newest_ts_us": 1_700_000_010_000_000,
            "ingest_accepted": 19,
            "ingest_dropped": {"log": 0, "telemetry": 1, "event": 0, "hw": 0},
            "unsynced": {"logs": 5, "metrics": 0, "events": 0, "hw": 0},
        },
        "page": {"next_cursor": None, "count": 1},
        "meta": {"source": "logd", "v": 1, "ts": 1, "db_lag_ms": 0},
    }


def test_group_registers_all_subcommands() -> None:
    result = runner.invoke(logs_group, ["--help"])
    assert result.exit_code == 0
    for sub in ("query", "tail", "aggregate", "export", "sessions", "status", "push"):
        assert sub in result.output


def test_query_help_lists_the_filter_options() -> None:
    result = runner.invoke(logs_group, ["query", "--help"])
    assert result.exit_code == 0
    for opt in ("--since", "--source", "--level", "--text", "--session", "--limit", "--cursor", "--json", "--host"):
        assert opt in result.output


def test_query_json_emits_the_raw_envelope() -> None:
    with patch.object(LogsClient, "get_json", return_value=_query_envelope()) as gj:
        result = runner.invoke(logs_group, ["query", "--kind", "logs", "--limit", "2", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    # The raw envelope is passed through verbatim.
    assert payload["meta"]["source"] == "logd"
    assert payload["page"]["next_cursor"] == "abc123"
    assert len(payload["data"]) == 2
    # The request hit /v1/query with the filters mapped onto query params.
    path, params = gj.call_args.args[0], gj.call_args.args[1]
    assert path == "/v1/query"
    assert params["kind"] == "logs"
    assert params["limit"] == 2


def test_query_human_output_renders_rows_and_cursor_hint() -> None:
    with patch.object(LogsClient, "get_json", return_value=_query_envelope()):
        result = runner.invoke(logs_group, ["query"])
    assert result.exit_code == 0, result.output
    assert "encoder stalled" in result.output
    assert "ados-video" in result.output
    # The "more" hint surfaces the next cursor for paging.
    assert "abc123" in result.output


def test_status_json_and_human() -> None:
    with patch.object(LogsClient, "get_json", return_value=_stats_envelope()):
        j = runner.invoke(logs_group, ["status", "--json"])
        h = runner.invoke(logs_group, ["status"])
    assert j.exit_code == 0
    assert json.loads(j.output)["data"]["integrity"] == "ok"
    assert h.exit_code == 0
    assert "integrity=ok" in h.output
    assert "logs=12" in h.output


def test_repeated_source_and_metric_filters_become_lists() -> None:
    with patch.object(LogsClient, "get_json", return_value=_query_envelope()) as gj:
        result = runner.invoke(
            logs_group,
            ["query", "--source", "api", "--source", "ados-video", "--text", "boom", "--json"],
        )
    assert result.exit_code == 0, result.output
    params = gj.call_args.args[1]
    assert params["source"] == ["api", "ados-video"]
    assert params["text"] == "boom"


def test_aggregate_requires_a_metric() -> None:
    result = runner.invoke(logs_group, ["aggregate"])
    # click reports the missing required option as a usage error.
    assert result.exit_code != 0
    assert "metric" in result.output.lower()


def test_aggregate_maps_bucket_and_agg() -> None:
    buckets = {
        "data": [{"bucket_us": 0, "metric": "cpu.load", "value": 0.5, "count": 3}],
        "page": {"next_cursor": None, "count": 1},
        "meta": {"source": "logd", "v": 1, "ts": 0, "db_lag_ms": 0},
    }
    with patch.object(LogsClient, "get_json", return_value=buckets) as gj:
        result = runner.invoke(logs_group, ["aggregate", "--metric", "cpu.load", "--bucket", "1m", "--agg", "p95"])
    assert result.exit_code == 0, result.output
    params = gj.call_args.args[1]
    assert params["metric"] == ["cpu.load"]
    assert params["bucket"] == "1m"
    assert params["agg"] == "p95"
    assert "cpu.load" in result.output


def test_status_openapi_fetches_the_schema_doc() -> None:
    doc = {"openapi": "3.0.3", "paths": {"/v1/query": {}}}
    with patch.object(LogsClient, "get_json", return_value=doc) as gj:
        result = runner.invoke(logs_group, ["status", "--openapi"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.output)["openapi"] == "3.0.3"
    assert gj.call_args.args[0] == "/v1/openapi.json"


# --- push (thin trigger front door) ------------------------------------


def test_push_help_lists_options() -> None:
    result = runner.invoke(logs_group, ["push", "--help"])
    assert result.exit_code == 0
    for opt in ("--session", "--since", "--kinds", "--no-wait", "--json"):
        assert opt in result.output


def test_push_writes_request_and_renders_pushed(tmp_path, monkeypatch) -> None:
    from ados.services.cloud import log_push_trigger as trig

    # Root takes the direct file seam (no loopback delegation).
    monkeypatch.setattr(os, "geteuid", lambda: 0)

    req = tmp_path / "logd-push-request.json"
    res = tmp_path / "logd-push-result.json"
    monkeypatch.setattr(trig, "ADOS_RUN_DIR", tmp_path)
    monkeypatch.setattr(trig, "LOGD_PUSH_REQUEST_PATH", req)
    monkeypatch.setattr(trig, "LOGD_PUSH_RESULT_PATH", res)

    # The cloud service "answers" by writing the result under the same id the
    # trigger picks. Pin the id so the harness can pre-stage the answer.
    monkeypatch.setattr(trig.uuid, "uuid4", lambda: _FakeUUID("rid-push"))

    real_write = trig.write_request

    def write_then_answer(request):
        rid = real_write(request)
        res.write_text(
            json.dumps({"request_id": rid, "pushed": True, "bytes": 4096, "rows": 12, "synced": True})
        )
        return rid

    monkeypatch.setattr(trig, "write_request", write_then_answer)

    result = runner.invoke(logs_group, ["push", "--session", "5", "--kinds", "logs"])
    assert result.exit_code == 0, result.output
    assert "pushed" in result.output
    on_disk = json.loads(req.read_text())
    assert on_disk["session"] == 5
    assert on_disk["kinds"] == ["logs"]


def test_push_no_wait_reports_requested(tmp_path, monkeypatch) -> None:
    from ados.services.cloud import log_push_trigger as trig

    monkeypatch.setattr(os, "geteuid", lambda: 0)
    monkeypatch.setattr(trig, "ADOS_RUN_DIR", tmp_path)
    monkeypatch.setattr(trig, "LOGD_PUSH_REQUEST_PATH", tmp_path / "req.json")
    monkeypatch.setattr(trig, "LOGD_PUSH_RESULT_PATH", tmp_path / "res.json")
    result = runner.invoke(logs_group, ["push", "--no-wait", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["pending"] is True
    assert payload["accepted"] is True


# --- push delegation (non-root → root agent over loopback) -------------


class _FakeResp:
    def __init__(self, status_code: int, payload: dict, text: str = "") -> None:
        self.status_code = status_code
        self._payload = payload
        self.text = text

    def json(self) -> dict:
        return self._payload


class _FakeClient:
    """Stand-in for httpx.Client as a context manager, capturing the POST."""

    def __init__(self, captured: dict, resp, raise_exc: Exception | None = None) -> None:
        self._captured = captured
        self._resp = resp
        self._raise = raise_exc

    def __enter__(self) -> _FakeClient:
        return self

    def __exit__(self, *_a) -> bool:
        return False

    def post(self, url, json=None, headers=None):  # noqa: A002 - httpx kw name
        self._captured["url"] = url
        self._captured["json"] = json
        self._captured["headers"] = headers
        if self._raise is not None:
            raise self._raise
        return self._resp


def test_push_non_root_delegates_to_loopback_api(monkeypatch) -> None:
    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setattr(logs_mod, "_load_api_key", lambda: None)
    captured: dict = {}
    resp = _FakeResp(200, {"pushed": True, "bytes": 4096, "rows": 12, "synced": True, "pending": False})
    monkeypatch.setattr(logs_mod.httpx, "Client", lambda *a, **k: _FakeClient(captured, resp))

    result = runner.invoke(logs_group, ["push", "--session", "7", "--kinds", "logs"])
    assert result.exit_code == 0, result.output
    assert "pushed" in result.output
    assert captured["url"].endswith("/api/logs/push")
    # Same-origin loopback header is the trust gate for a non-root caller.
    assert captured["headers"]["Origin"] == "http://localhost:8080"
    assert "X-ADOS-Key" not in captured["headers"]
    assert captured["json"]["session"] == 7
    assert captured["json"]["kinds"] == ["logs"]
    assert captured["json"]["wait"] is True


def test_push_non_root_falls_back_to_seam_when_api_down(tmp_path, monkeypatch) -> None:
    from ados.services.cloud import log_push_trigger as trig

    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setattr(logs_mod, "_load_api_key", lambda: None)
    monkeypatch.setattr(
        logs_mod.httpx,
        "Client",
        lambda *a, **k: _FakeClient({}, None, raise_exc=logs_mod.httpx.ConnectError("down")),
    )

    req = tmp_path / "req.json"
    res = tmp_path / "res.json"
    monkeypatch.setattr(trig, "ADOS_RUN_DIR", tmp_path)
    monkeypatch.setattr(trig, "LOGD_PUSH_REQUEST_PATH", req)
    monkeypatch.setattr(trig, "LOGD_PUSH_RESULT_PATH", res)
    monkeypatch.setattr(trig.uuid, "uuid4", lambda: _FakeUUID("rid-fb"))

    real_write = trig.write_request

    def write_then_answer(request):
        rid = real_write(request)
        res.write_text(json.dumps({"request_id": rid, "pushed": True, "bytes": 1, "rows": 1, "synced": True}))
        return rid

    monkeypatch.setattr(trig, "write_request", write_then_answer)

    result = runner.invoke(logs_group, ["push", "--kinds", "logs"])
    assert result.exit_code == 0, result.output
    assert "pushed" in result.output
    assert json.loads(req.read_text())["kinds"] == ["logs"]


def test_push_non_root_api_down_and_seam_denied_hints_sudo(monkeypatch) -> None:
    from ados.services.cloud import log_push_trigger as trig

    monkeypatch.setattr(os, "geteuid", lambda: 1000)
    monkeypatch.setattr(logs_mod, "_load_api_key", lambda: None)
    monkeypatch.setattr(
        logs_mod.httpx,
        "Client",
        lambda *a, **k: _FakeClient({}, None, raise_exc=logs_mod.httpx.ConnectError("down")),
    )

    def deny(_request, *, wait):
        raise trig.LogPushTriggerError(
            "trigger_unavailable", "could not write the push request: [Errno 13] Permission denied"
        )

    monkeypatch.setattr(trig, "trigger_push", deny)

    result = runner.invoke(logs_group, ["push", "--no-wait"])
    assert result.exit_code != 0
    assert "run with sudo" in result.output


def test_push_rejects_bad_since() -> None:
    result = runner.invoke(logs_group, ["push", "--since", "yesterday"])
    assert result.exit_code != 0
    assert "bad_since" in result.output


class _FakeUUID:
    def __init__(self, hexval: str) -> None:
        self.hex = hexval


# --- transport resolution ----------------------------------------------


def test_on_box_default_uses_the_unix_socket_with_no_key() -> None:
    client = LogsClient(socket_path="/run/ados/logd-query.sock", host=None, port=8090, key=None)
    # No host → a uds transport is built and no auth header is set.
    assert client._transport is not None
    assert "X-ADOS-Key" not in client._headers


def test_off_box_uses_tcp_and_sends_the_explicit_key() -> None:
    client = LogsClient(socket_path="/run/ados/logd-query.sock", host="192.168.1.50", port=8090, key="ados_explicit")
    assert client._transport is None
    assert client._base_url == "http://192.168.1.50:8090"
    assert client._headers["X-ADOS-Key"] == "ados_explicit"


def test_off_box_falls_back_to_the_env_key(monkeypatch) -> None:
    monkeypatch.setenv("ADOS_KEY", "ados_from_env")
    client = LogsClient(socket_path="/run/ados/logd-query.sock", host="host", port=8090, key=None)
    assert client._headers["X-ADOS-Key"] == "ados_from_env"


# --- real unix-socket transport (no mocks) ------------------------------


def _stub_unix_server(tmp_path):
    """A tiny HTTP server bound to a unix socket that answers the three call
    shapes the CLI makes: a JSON envelope on /v1/query, a streamed jsonl body on
    /v1/export, and an SSE stream on /v1/tail. Returns (socket_path, server)."""
    import http.server
    import json as _json
    import os
    import socketserver

    sock = os.path.join(tmp_path, "logd-query.sock")

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_a):  # silence the test server
            pass

        def do_GET(self):  # noqa: N802 - BaseHTTPRequestHandler API
            if self.path.startswith("/v1/query"):
                body = _json.dumps(_query_envelope()).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            elif self.path.startswith("/v1/export"):
                lines = b'{"id":1,"msg":"a"}\n{"id":2,"msg":"b"}\n'
                self.send_response(200)
                self.send_header("Content-Type", "application/x-ndjson")
                self.send_header("Content-Length", str(len(lines)))
                self.end_headers()
                self.wfile.write(lines)
            elif self.path.startswith("/v1/tail"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(b'data: {"kind":"log","msg":"live one"}\n\n')
                self.wfile.write(b'data: {"kind":"lagged","dropped":2}\n\n')
                # End the stream so the client iterator completes.
            else:
                self.send_response(404)
                self.end_headers()

    class UnixHTTPServer(socketserver.UnixStreamServer):
        def get_request(self):
            conn, _ = self.socket.accept()
            return conn, ("localhost", 0)

    server = UnixHTTPServer(sock, Handler)
    return sock, server


def test_real_unix_socket_query_export_and_tail(tmp_path) -> None:
    import threading

    sock, server = _stub_unix_server(str(tmp_path))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # JSON GET over the real uds transport.
        with LogsClient(socket_path=sock, host=None, port=8090, key=None) as client:
            env = client.get_json("/v1/query", {"limit": 2})
        assert env["meta"]["source"] == "logd"
        assert env["page"]["count"] == 2

        # Export: the raw body streams back as bytes.
        with LogsClient(socket_path=sock, host=None, port=8090, key=None) as client:
            body = b"".join(client.stream("/v1/export", {"format": "jsonl"}))
        assert body.count(b"\n") == 2

        # Tail: the SSE data payloads decode to events (incl. the lagged note).
        with LogsClient(socket_path=sock, host=None, port=8090, key=None) as client:
            events = list(client.stream_sse("/v1/tail", {}))
        kinds = [e.get("kind") for e in events]
        assert "log" in kinds
        assert "lagged" in kinds
    finally:
        server.shutdown()
        server.server_close()


def test_off_box_falls_back_to_the_pairing_key(monkeypatch, tmp_path) -> None:
    monkeypatch.delenv("ADOS_KEY", raising=False)
    pairing = tmp_path / "pairing.json"
    pairing.write_text(json.dumps({"paired": True, "api_key": "ados_from_file"}))
    with patch("ados.cli.logs_transport.PAIRING_JSON", pairing, create=True):
        # The loader imports PAIRING_JSON inside the function, so patch the
        # source module path it imports from.
        with patch("ados.core.paths.PAIRING_JSON", pairing):
            client = LogsClient(socket_path="/run/ados/logd-query.sock", host="host", port=8090, key=None)
    assert client._headers["X-ADOS-Key"] == "ados_from_file"
