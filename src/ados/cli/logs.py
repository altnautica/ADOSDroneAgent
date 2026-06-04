"""``ados logs`` CLI group: the durable local-store front door.

Today an operator (or a coding agent over SSH) debugs a field failure with
``journalctl`` and ``grep``, which is fragile exactly when the network drops.
This group queries the durable local store instead: it survives reboots, is
reachable when the network is down, and holds logs from every process plus
telemetry, events, and hardware history.

Transport is local-first (same stance the rest of the agent takes):

* on-box, the default is the trusted unix query socket at
  ``/run/ados/logd-query.sock`` — no key needed, and it answers even if the
  Python API on :8080 is down, because the logging daemon serves it directly.
* off-box, ``--host <ip>`` switches to the LAN TCP port :8090 and sends an
  ``X-ADOS-Key`` read from ``--key``, the ``ADOS_KEY`` env var, or the local
  pairing file, in that order.

Every subcommand accepts ``--json``, which prints the raw query-API envelope
verbatim — the stable contract a coding agent consumes. The human default is a
colorized table and is explicitly not a stable contract.
"""

from __future__ import annotations

import json
import os
import sys
from typing import Any

import click
import httpx

from ados.cli.logs_transport import LogsClient, LogsTransportError
from ados.core.paths import LOGD_QUERY_SOCK, PAIRING_JSON

# The query-API port and unix socket the logging daemon binds. Kept in step
# with the daemon's runtime paths (crates/ados-logd/src/lib.rs).
QUERY_SOCKET = str(LOGD_QUERY_SOCK)
QUERY_TCP_PORT = 8090

# The agent's local control surface. `push` records its request under the
# root-owned runtime dir, so a non-root operator hands the write to the agent
# process here instead of touching the dir itself.
API_BASE = "http://localhost:8080"


def _client(host: str | None, key: str | None) -> LogsClient:
    """Build the transport for the resolved plane (unix on-box, TCP off-box)."""
    return LogsClient(
        socket_path=QUERY_SOCKET,
        host=host,
        port=QUERY_TCP_PORT,
        key=key,
    )


def _emit(envelope: dict[str, Any], as_json: bool, render) -> None:
    """Print either the raw envelope (``--json``) or the human rendering."""
    if as_json:
        click.echo(json.dumps(envelope, indent=2))
        return
    render(envelope)


def _run(host: str | None, key: str | None, path: str, params: dict[str, Any]):
    """Issue one GET against the query API, raising a click error on failure."""
    try:
        with _client(host, key) as client:
            return client.get_json(path, params)
    except LogsTransportError as exc:
        raise click.ClickException(str(exc)) from exc


# Shared option decorators. Defined once so every subcommand carries the same
# transport flags with the same help text.
def _transport_options(fn):
    fn = click.option(
        "--host",
        default=None,
        help="Query a remote agent over the LAN (TCP :8090). Default is the on-box unix socket.",
    )(fn)
    fn = click.option(
        "--key",
        default=None,
        help="API key for --host (else ADOS_KEY env, else the local pairing key).",
    )(fn)
    fn = click.option("--json", "as_json", is_flag=True, help="Print the raw query-API envelope.")(fn)
    return fn


@click.group("logs")
def logs_group() -> None:
    """Query the durable local logging and telemetry store."""


@logs_group.command("query")
@click.option("--since", default=None, help="Relative lower bound, e.g. -5m, -2h, -1d.")
@click.option("--from", "from_", default=None, help="Window start (epoch us, ISO, or -5m).")
@click.option("--to", "to_", default=None, help="Window end (epoch us, ISO, or -5m).")
@click.option("--kind", default=None, type=click.Choice(["logs", "events", "metrics", "hw"]))
@click.option("--source", multiple=True, help="Filter by emitting source (repeatable).")
@click.option("--metric", multiple=True, help="Filter by metric key (repeatable; for metrics).")
@click.option("--event-kind", "event_kind", multiple=True, help="Filter by event kind (repeatable).")
@click.option("--level", default=None, help="Minimum level: trace|debug|info|warn|error.")
@click.option("--text", default=None, help="Substring match on the message.")
@click.option("--session", default=None, type=int, help="Restrict to one session id.")
@click.option("--limit", default=None, type=int, help="Page size.")
@click.option("--cursor", default=None, help="Pagination cursor from a prior page.")
@_transport_options
def query_cmd(  # noqa: PLR0913 - the filter surface maps 1:1 onto the query API
    since, from_, to_, kind, source, metric, event_kind, level, text, session, limit, cursor,
    host, key, as_json,
) -> None:
    """Keyset-paginated rows across the logs, events, metrics, or hardware tables."""
    params: dict[str, Any] = {}
    _put(params, "since", since)
    _put(params, "from", from_)
    _put(params, "to", to_)
    _put(params, "kind", kind)
    _put(params, "level", level)
    _put(params, "text", text)
    _put(params, "session", session)
    _put(params, "limit", limit)
    _put(params, "cursor", cursor)
    _put_multi(params, "source", source)
    _put_multi(params, "metric", metric)
    _put_multi(params, "event_kind", event_kind)
    envelope = _run(host, key, "/v1/query", params)
    _emit(envelope, as_json, _render_rows)


@logs_group.command("aggregate")
@click.option("--metric", multiple=True, required=True, help="Metric key (repeatable).")
@click.option("--since", default=None, help="Relative lower bound, e.g. -1h.")
@click.option("--from", "from_", default=None, help="Window start.")
@click.option("--to", "to_", default=None, help="Window end.")
@click.option("--session", default=None, type=int, help="Restrict to one session id.")
@click.option("--bucket", default="auto", type=click.Choice(["auto", "1s", "1m", "1h"]))
@click.option("--agg", default="avg", type=click.Choice(["avg", "min", "max", "p50", "p95", "last", "count"]))
@_transport_options
def aggregate_cmd(metric, since, from_, to_, session, bucket, agg, host, key, as_json) -> None:
    """Downsampled metric series for charts."""
    params: dict[str, Any] = {"bucket": bucket, "agg": agg}
    _put(params, "since", since)
    _put(params, "from", from_)
    _put(params, "to", to_)
    _put(params, "session", session)
    _put_multi(params, "metric", metric)
    envelope = _run(host, key, "/v1/aggregate", params)
    _emit(envelope, as_json, _render_buckets)


@logs_group.command("sessions")
@click.option("--since", default=None, help="Relative lower bound on the start time.")
@click.option("--from", "from_", default=None, help="Window start.")
@click.option("--to", "to_", default=None, help="Window end.")
@click.option("--kind", default=None, type=click.Choice(["flight", "boot", "manual"]))
@click.option("--open", "open_only", is_flag=True, help="Only currently-open sessions.")
@click.option("--limit", default=None, type=int, help="Page size.")
@_transport_options
def sessions_cmd(since, from_, to_, kind, open_only, limit, host, key, as_json) -> None:
    """List boot, flight, and manual sessions with per-session counts."""
    params: dict[str, Any] = {}
    _put(params, "from", from_ or since)
    _put(params, "to", to_)
    _put(params, "kind", kind)
    _put(params, "limit", limit)
    if open_only:
        params["open"] = "true"
    envelope = _run(host, key, "/v1/sessions", params)
    _emit(envelope, as_json, _render_sessions)


@logs_group.command("status")
@click.option("--openapi", is_flag=True, help="Print the query-API OpenAPI schema instead.")
@_transport_options
def status_cmd(openapi, host, key, as_json) -> None:
    """Store health, ingest and drop rates, and the sync watermark."""
    if openapi:
        doc = _run(host, key, "/v1/openapi.json", {})
        click.echo(json.dumps(doc, indent=2))
        return
    envelope = _run(host, key, "/v1/stats", {})
    _emit(envelope, as_json, _render_stats)


@logs_group.command("export")
@click.option("--since", default=None, help="Relative lower bound, e.g. -1h.")
@click.option("--from", "from_", default=None, help="Window start.")
@click.option("--to", "to_", default=None, help="Window end.")
@click.option("--kind", default=None, type=click.Choice(["logs", "events", "metrics", "hw"]))
@click.option("--source", multiple=True, help="Filter by source (repeatable).")
@click.option("--metric", multiple=True, help="Filter by metric (repeatable).")
@click.option("--session", default=None, type=int, help="Restrict to one session id.")
@click.option("--format", "fmt", default="jsonl", type=click.Choice(["jsonl", "jsonl.zst"]))
@click.option("--output", "-o", "output", default=None, type=click.Path(), help="Write to a file (default stdout).")
@click.option("--host", default=None, help="Query a remote agent over the LAN (TCP :8090).")
@click.option("--key", default=None, help="API key for --host.")
def export_cmd(since, from_, to_, kind, source, metric, session, fmt, output, host, key) -> None:  # noqa: PLR0913
    """Stream a window of the store as jsonl or jsonl.zst to a file or stdout."""
    params: dict[str, Any] = {"format": fmt}
    _put(params, "since", since)
    _put(params, "from", from_)
    _put(params, "to", to_)
    _put(params, "kind", kind)
    _put(params, "session", session)
    _put_multi(params, "source", source)
    _put_multi(params, "metric", metric)
    try:
        with _client(host, key) as client:
            sink = open(output, "wb") if output else None  # noqa: SIM115 - closed in finally
            try:
                written = 0
                for chunk in client.stream("/v1/export", params):
                    if sink is not None:
                        sink.write(chunk)
                    else:
                        sys.stdout.buffer.write(chunk)
                    written += len(chunk)
            finally:
                if sink is not None:
                    sink.close()
    except LogsTransportError as exc:
        raise click.ClickException(str(exc)) from exc
    if output:
        click.echo(f"Wrote {written} bytes to {output}", err=True)


def _load_api_key() -> str | None:
    """Read the local pairing api_key, when this caller can read the file.

    The pairing file is owned by the root agent process (``0600``), so a
    non-root operator gets ``None`` here — which is expected. The same-origin
    header in :func:`_delegate_push_via_api` is what authorises the loopback
    request on a paired agent; the key is attached only when readable (a root
    caller) so the strict path also works.
    """
    try:
        if PAIRING_JSON.exists():
            data = json.loads(PAIRING_JSON.read_text(encoding="utf-8"))
            key = data.get("api_key")
            return key if isinstance(key, str) else None
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _delegate_push_via_api(request, *, wait: bool) -> dict[str, Any] | None:
    """Hand the push request to the root agent over its local control surface.

    The request file lives under the root-owned runtime dir, so a non-root
    operator cannot write it directly. Rather than widen that directory, the
    operator posts the request to the agent on loopback; the agent process
    (running as root) performs the privileged write and returns the same result
    envelope the direct seam would. This mirrors how the sibling ``logs``
    subcommands reach the daemon, and it rides the eventual move of this route
    onto the native control surface unchanged.

    Returns the result envelope, or ``None`` when the local API is unreachable
    or cannot authorise the request, so the caller can fall back to the direct
    seam (and, for a non-root caller, surface a clear hint).
    """
    body: dict[str, Any] = {"kinds": list(request.kinds), "wait": wait}
    if request.session is not None:
        body["session"] = request.session
    if request.since_us is not None:
        # An epoch-microsecond integer is one of the lower-bound forms the
        # route accepts, so the already-resolved bound round-trips verbatim.
        body["since"] = request.since_us

    # Being on the box, reaching loopback, is the trust gate: the same-origin
    # header authorises a keyless request on a paired agent exactly as the
    # agent-served dashboard is trusted, and the key is added too when readable.
    headers = {"Origin": API_BASE}
    key = _load_api_key()
    if key:
        headers["X-ADOS-Key"] = key

    try:
        with httpx.Client(timeout=12.0) as client:
            resp = client.post(f"{API_BASE}/api/logs/push", json=body, headers=headers)
    except httpx.ConnectError:
        return None
    except httpx.HTTPError as exc:
        raise click.ClickException(str(exc)) from exc

    if resp.status_code in (200, 202):
        data = resp.json()
        return data if isinstance(data, dict) else None
    if resp.status_code == 401:
        # Paired and hardened (same-origin trust closed) with no readable key:
        # nothing more the CLI can do without privilege. Fall back.
        return None
    # A rejected selector or other error: surface the route's structured error.
    try:
        err = resp.json().get("error") or {}
    except ValueError:
        err = {}
    code = err.get("code", f"http_{resp.status_code}")
    message = err.get("message", resp.text[:160])
    raise click.ClickException(f"{code}: {message}")


@logs_group.command("push")
@click.option("--session", default=None, type=int, help="Restrict the window to one session id.")
@click.option("--since", default=None, help="Lower bound: a relative -5m/-2h/-1d, an epoch-us integer, or an ISO timestamp.")
@click.option(
    "--kinds",
    default=None,
    help="Comma-separated subset of logs,metrics,events,hw. Default: all four.",
)
@click.option("--no-wait", is_flag=True, help="Return as soon as the push is requested, without waiting for the result.")
@click.option("--json", "as_json", is_flag=True, help="Print the raw result envelope.")
def push_cmd(session, since, kinds, no_wait, as_json) -> None:
    """Export a window of the store to the paired cloud account.

    This is a thin front door: it records the request and lets the cloud service
    do the export, upload, and mark. The cloud service refuses the push when the
    agent is in local mode, is not cloud-paired, or has cloud log push disabled,
    which is the correct state for an agent that has nothing to sync. By default
    the command waits a few seconds for the result and prints what was pushed;
    ``--no-wait`` returns as soon as the request is recorded.

    The request file lives under the root-owned runtime dir: root records it
    directly, while a non-root operator hands the write to the running agent on
    loopback and only falls back to the direct seam if the agent is unreachable.
    """
    from ados.services.cloud.log_push_trigger import (
        LogPushTriggerError,
        build_request,
        trigger_push,
    )

    kind_list = [k.strip() for k in kinds.split(",") if k.strip()] if kinds else None
    try:
        request = build_request(session=session, since=since, kinds=kind_list)
    except LogPushTriggerError as exc:
        raise click.ClickException(f"{exc.code}: {exc.message}") from exc

    result: dict[str, Any] | None = None
    if os.geteuid() != 0:
        result = _delegate_push_via_api(request, wait=not no_wait)
    if result is None:
        try:
            result = trigger_push(request, wait=not no_wait)
        except LogPushTriggerError as exc:
            if exc.code == "trigger_unavailable" and os.geteuid() != 0:
                raise click.ClickException(
                    "the local agent API is not reachable and you are not root; "
                    "start the agent or run with sudo"
                ) from exc
            raise click.ClickException(f"{exc.code}: {exc.message}") from exc

    if as_json:
        click.echo(json.dumps(result, indent=2))
        return
    _render_push(result)


def _render_push(result: dict[str, Any]) -> None:
    """Render the push outcome as a one-line human summary."""
    if result.get("error"):
        click.echo(click.style(f"push failed: {result['error']}", fg="red"))
        return
    if result.get("pending"):
        click.echo(
            click.style("push requested", fg="cyan")
            + " — the cloud service will export the window shortly."
        )
        return
    if result.get("deduped"):
        click.echo(
            click.style("already pushed", fg="green")
            + f" — {result.get('rows', 0)} rows, {result.get('bytes', 0)} bytes (no new upload)."
        )
        return
    synced = "synced" if result.get("synced") else "uploaded (mark pending)"
    click.echo(
        click.style("pushed", fg="green")
        + f" — {result.get('rows', 0)} rows, {result.get('bytes', 0)} bytes, {synced}."
    )


@logs_group.command("tail")
@click.option("--replay", default=0, type=int, help="Replay this many recent rows before the live tail.")
@click.option("--kind", default=None, type=click.Choice(["logs", "events", "metrics", "hw"]))
@click.option("--source", multiple=True, help="Filter by source (repeatable).")
@click.option("--metric", multiple=True, help="Filter by metric (repeatable).")
@click.option("--event-kind", "event_kind", multiple=True, help="Filter by event kind (repeatable).")
@click.option("--level", default=None, help="Minimum level.")
@click.option("--text", default=None, help="Substring match on the message.")
@_transport_options
def tail_cmd(replay, kind, source, metric, event_kind, level, text, host, key, as_json) -> None:  # noqa: PLR0913
    """Follow the live stream of new rows until interrupted."""
    params: dict[str, Any] = {}
    if replay:
        params["replay"] = replay
    _put(params, "kind", kind)
    _put(params, "level", level)
    _put(params, "text", text)
    _put_multi(params, "source", source)
    _put_multi(params, "metric", metric)
    _put_multi(params, "event_kind", event_kind)
    try:
        with _client(host, key) as client:
            for event in client.stream_sse("/v1/tail", params):
                if as_json:
                    click.echo(json.dumps(event))
                else:
                    _render_tail_event(event)
    except LogsTransportError as exc:
        raise click.ClickException(str(exc)) from exc
    except KeyboardInterrupt:  # pragma: no cover - interactive interrupt
        pass


# --- request helpers ----------------------------------------------------


def _put(params: dict[str, Any], name: str, value: Any) -> None:
    """Set a single query param when the value is present."""
    if value is not None and value != "":
        params[name] = value


def _put_multi(params: dict[str, Any], name: str, values) -> None:
    """Set a repeated query param when at least one value is present."""
    items = [v for v in (values or ()) if v]
    if items:
        params[name] = list(items)


# --- human rendering (not a stable contract) ----------------------------

_LEVEL_COLOR = {
    "trace": "bright_black",
    "debug": "cyan",
    "info": "green",
    "warn": "yellow",
    "error": "red",
}


def _ts(us: Any) -> str:
    """Render a microsecond-epoch integer as a short local time-of-day stamp."""
    try:
        import datetime as _dt

        return _dt.datetime.fromtimestamp(int(us) / 1_000_000).strftime("%H:%M:%S.%f")[:-3]
    except (TypeError, ValueError, OSError):
        return str(us)


def _render_rows(envelope: dict[str, Any]) -> None:
    rows = envelope.get("data") or []
    if not rows:
        click.echo("(no rows)")
        return
    for r in rows:
        _render_one_row(r)
    cursor = (envelope.get("page") or {}).get("next_cursor")
    if cursor:
        click.echo(click.style(f"  …more — page with --cursor {cursor}", fg="bright_black"))


def _render_one_row(r: dict[str, Any]) -> None:
    """Render a single row, dispatching on the columns the table provides."""
    ts = _ts(r.get("ts_us"))
    if "metric" in r and "value" in r:
        click.echo(f"{ts}  {r['metric']:<24} {r['value']}")
    elif "signals" in r:
        click.echo(f"{ts}  hw  {json.dumps(r['signals'])}")
    elif "kind" in r and "severity" in r:
        sev = str(r.get("severity", ""))
        coloured = click.style(sev.upper().ljust(5), fg=_LEVEL_COLOR.get(sev))
        click.echo(f"{ts}  {coloured}  {r['kind']}  {r.get('source', '')}")
    else:
        lvl = str(r.get("level", ""))
        coloured = click.style(lvl.upper().ljust(5), fg=_LEVEL_COLOR.get(lvl))
        source = str(r.get("source", "")).ljust(14)
        click.echo(f"{ts}  {coloured}  {source} {r.get('msg', '')}")


def _render_buckets(envelope: dict[str, Any]) -> None:
    rows = envelope.get("data") or []
    if not rows:
        click.echo("(no data)")
        return
    for b in rows:
        click.echo(f"{_ts(b.get('bucket_us'))}  {b.get('metric', ''):<24} {b.get('value')}  (n={b.get('count')})")


def _render_sessions(envelope: dict[str, Any]) -> None:
    rows = envelope.get("data") or []
    if not rows:
        click.echo("(no sessions)")
        return
    for s in rows:
        ended = _ts(s["ended_us"]) if s.get("ended_us") is not None else "open"
        click.echo(
            f"#{s.get('id')}  {s.get('kind', ''):<7} {_ts(s.get('started_us'))} → {ended}  "
            f"logs={s.get('log_count')} events={s.get('event_count')}"
        )


def _render_stats(envelope: dict[str, Any]) -> None:
    d = envelope.get("data") or {}
    rows = d.get("rows", {})
    click.echo(f"DB size:    {d.get('db_size_bytes', 0)} bytes  (wal {d.get('wal_size_bytes', 0)})")
    click.echo(f"Schema:     v{d.get('schema_version')}  integrity={d.get('integrity')}")
    click.echo("Rows:       " + "  ".join(f"{k}={v}" for k, v in rows.items()))
    click.echo(f"Ingest:     accepted={d.get('ingest_accepted')}  dropped={d.get('ingest_dropped')}")
    click.echo(f"Unsynced:   {d.get('unsynced')}")
    span = (d.get("oldest_ts_us"), d.get("newest_ts_us"))
    if span[0] is not None and span[1] is not None:
        click.echo(f"Span:       {_ts(span[0])} → {_ts(span[1])}")


def _render_tail_event(event: dict[str, Any]) -> None:
    if event.get("kind") == "lagged":
        click.echo(click.style(f"  (lagged, dropped {event.get('dropped')})", fg="yellow"))
        return
    _render_one_row(event)


__all__ = ["logs_group"]


# Allow `python -m ados.cli.logs` for quick local checks.
if __name__ == "__main__":  # pragma: no cover
    logs_group()
