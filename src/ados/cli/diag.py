"""``ados diag`` CLI subcommand tree — reliable, per-hop pipeline diagnosis.

Two operator-facing commands, both thin renderers over the agent's native REST
surface (the logic lives in the Rust control front, so the CLI shows exactly what
Mission Control sees):

* ``ados diag video`` — a per-hop verdict on the video pipeline (``/api/diag/video``).
  Each hop is judged by reliable cumulative/rate counters sampled over a window,
  NEVER by tcpdump / queue depth / a single snapshot, and the first real break is
  named as ``video dies at``.
* ``ados diag link`` — the WFB link diagnosis (``/api/wfb``): the one-glance
  ``link_diag`` verdict (deaf / mis_keyed / jammed / healthy / searching) plus the
  decode counters that separate the failure modes a bare "0 received" hides.

Reach for these BEFORE hand-probing a pipeline (the reliable-diagnostics rule).
"""

from __future__ import annotations

import json
from typing import Any

import click
import httpx

from ados.cli import _ansi, api_bases
from ados.core.paths import PAIRING_JSON


def _load_api_key() -> str | None:
    try:
        if PAIRING_JSON.exists():
            data = json.loads(PAIRING_JSON.read_text(encoding="utf-8"))
            key = data.get("api_key")
            return key if isinstance(key, str) else None
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _auth_headers() -> dict[str, str]:
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _request(method: str, path: str, **kwargs: Any) -> dict[str, Any]:
    """Minimal REST helper. Returns the parsed body dict.

    Tries each candidate control port so a wrong-port probe never reads as
    "agent not running"; a connection refusal falls through to the next.
    """
    timeout = kwargs.pop("timeout", 10.0)
    try:
        for base in api_bases():
            try:
                with httpx.Client(timeout=timeout) as client:
                    resp = client.request(
                        method, f"{base}{path}", headers=_auth_headers(), **kwargs
                    )
                if resp.status_code >= 400:
                    raise click.ClickException(
                        f"Agent API returned {resp.status_code} for {path}"
                    )
                try:
                    body = resp.json()
                except ValueError as exc:
                    raise click.ClickException(
                        f"Agent API returned non-JSON for {path}"
                    ) from exc
                return body if isinstance(body, dict) else {"data": body}
            except httpx.ConnectError:
                continue  # this port refused; try the next candidate
        raise click.ClickException(
            "Agent is not running. Start the supervisor or run `ados demo`."
        )
    except httpx.HTTPError as exc:
        raise click.ClickException(str(exc)) from exc


# Map a video-hop verdict to a state dot + a plain colour for the summary line.
_HOP_STATE = {
    "flowing": "ok",
    "stalled": "fail",
    "no_upstream": "pending",
    "unknown": "warn",
}
# Map a WFB link_diag verdict to a state dot.
_LINK_STATE = {
    "healthy": "ok",
    "searching": "pending",
    "deaf": "fail",
    "mis_keyed": "fail",
    "jammed": "warn",
}


@click.group("diag", help="Reliable per-hop pipeline diagnosis (video, link).")
def diag_group() -> None:
    pass


@diag_group.command("video", help="Per-hop verdict on the video pipeline.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def diag_video(as_json: bool) -> None:
    data = _request("GET", "/api/diag/video")
    if as_json:
        click.echo(json.dumps(data, indent=2, sort_keys=True))
        return

    theme = _ansi.detect_theme()
    profile = data.get("profile", "?")
    role = data.get("role", "?")
    window = data.get("window_s")
    hops = data.get("hops") or []
    dies_at = data.get("video_dies_at")

    click.echo(_ansi.marker(theme, "VIDEO PIPELINE"))
    win = f"  ·  {window:.1f}s window" if isinstance(window, (int, float)) else ""
    click.echo(_ansi.kv(theme, "profile", f"{profile} / {role}{win}"))
    click.echo("")

    for hop in hops:
        verdict = str(hop.get("verdict", "unknown"))
        state = _HOP_STATE.get(verdict, "warn")
        label = str(hop.get("label", hop.get("name", "?")))
        metric = str(hop.get("metric", ""))
        method = str(hop.get("method", ""))
        detail = hop.get("detail")
        click.echo(
            f"  {_ansi.dot(theme, state)} {theme.bold(label)}  "
            f"{theme.dim(verdict.upper())}"
        )
        click.echo(_ansi.kv(theme, "", metric, label_width=2))
        if method:
            click.echo(_ansi.kv(theme, "", theme.dim(f"via {method}"), label_width=2))
        if isinstance(detail, str) and detail:
            click.echo(_ansi.kv(theme, "", theme.dim(detail), label_width=2))
        click.echo("")

    if isinstance(dies_at, str) and dies_at:
        click.echo(f"  {theme.fail(theme.glyph_fail())} video dies at: {theme.bold(dies_at)}")
    elif hops:
        click.echo(f"  {theme.ok(theme.glyph_ok())} all hops flowing")
    else:
        click.echo(f"  {theme.dim('no hops reported (pipeline not running?)')}")


@diag_group.command("link", help="WFB link diagnosis (deaf / mis_keyed / jammed / healthy).")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def diag_link(as_json: bool) -> None:
    data = _request("GET", "/api/wfb")
    if as_json:
        click.echo(json.dumps(data, indent=2, sort_keys=True))
        return

    theme = _ansi.detect_theme()
    link_diag = data.get("link_diag")
    state = data.get("state", "?")

    click.echo(_ansi.marker(theme, "WFB LINK"))
    if isinstance(link_diag, str) and link_diag:
        dot = _ansi.dot(theme, _LINK_STATE.get(link_diag, "warn"))
        click.echo(f"  {dot} {theme.bold(link_diag.upper())}  {theme.dim(f'(state={state})')}")
    else:
        # No verdict field on this reading — report the lifecycle state honestly
        # rather than inventing a verdict.
        click.echo(f"  {_ansi.dot(theme, 'pending')} {theme.bold(str(state).upper())}")
    click.echo("")

    def _num(key: str) -> str:
        v = data.get(key)
        return str(v) if isinstance(v, (int, float)) else "—"

    rssi = data.get("rssi_dbm")
    rssi_str = f"{rssi:.0f} dBm" if isinstance(rssi, (int, float)) else "— (no decode)"
    rows = [
        ("RSSI", rssi_str),
        ("Channel", _num("channel")),
        ("Decoded pkt/s", _num("packets_received")),
        ("RF frames (all)", _num("packets_all")),
        ("Decrypt errors", _num("decrypt_errors")),
        ("Bad packets", _num("packets_bad")),
        ("FEC recovered", _num("fec_recovered")),
        ("Bitrate", f"{data.get('bitrate_mbps')} Mbps" if data.get("bitrate_mbps") else "—"),
    ]
    for label, value in rows:
        click.echo(_ansi.kv(theme, label, value, label_width=16))
