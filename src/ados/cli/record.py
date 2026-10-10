"""``ados record`` CLI subcommand tree — the recorder, reachable over SSH.

Four operator-facing commands, all thin clients over the agent's ground-station
recording routes (the logic lives in the Rust control front, so the CLI shows
exactly what Mission Control sees):

* ``ados record start`` — begin a capture (``POST .../recording/start``).
* ``ados record stop`` — end the in-flight capture (``POST .../recording/stop``).
* ``ados record list`` — the on-disk captures plus the live recording flag
  (``GET .../recording/list``), or, with ``--path``, mediamtx's playback segment
  inventory for one stream (``GET .../recording/segments``).
* ``ados record clip`` — cut an fMP4 clip out of mediamtx's recorded segments
  and write it to a file (``GET .../recording/clip``), STREAMED to disk.

Every route is ground-station-profile gated, so running these on a drone answers
a profile mismatch — which this reports as a sentence rather than a raw error
body, because "wrong box" is the actual diagnosis.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import click
import httpx

from ados.cli import _agent_http, _ansi

# Recording payloads are a whole video file, so the clip read gets a long
# deadline while the JSON reads keep the snappy one the sibling verbs use.
_JSON_TIMEOUT = 10.0
_CLIP_TIMEOUT = 300.0

_RECORDING_BASE = "/api/v1/ground-station/recording"


def _error_code_and_message(body: dict[str, Any]) -> tuple[str | None, str | None]:
    """Pull ``(code, message)`` out of the recording routes' error envelope.

    The whole recording surface answers ``{"detail": {"error": {"code",
    "message"}}}`` — the FastAPI error-OBJECT shape, not the bare-string
    ``detail`` the rest of the API uses — so one reader covers every failure the
    four verbs can hit.
    """
    detail = body.get("detail")
    if isinstance(detail, dict):
        error = detail.get("error")
        if isinstance(error, dict):
            code = error.get("code")
            message = error.get("message")
            return (
                code if isinstance(code, str) else None,
                message if isinstance(message, str) else None,
            )
    return None, None


def _raise_api_error(status: int, body: dict[str, Any]) -> None:
    """Turn a 4xx/5xx recording response into an operator-readable failure."""
    code, message = _error_code_and_message(body)
    if code == "E_PROFILE_MISMATCH":
        raise click.ClickException(
            "This node is not a ground station. `ados record` drives the ground "
            "node's recorder; run it there."
        )
    if code == "E_PLAYBACK_UNAVAILABLE":
        raise click.ClickException(
            "mediamtx's playback server is not answering. Check the video service "
            "with `ados diag video`."
        )
    if code and message:
        raise click.ClickException(f"{code}: {message}")
    if code:
        raise click.ClickException(code)
    raise click.ClickException(f"Agent API returned {status}: {body.get('detail') or body}")


def _as_dict(body: Any) -> dict[str, Any]:
    if isinstance(body, str):
        return {"text": body}
    return body if isinstance(body, dict) else {"data": body}


def _request(
    method: str,
    path: str,
    *,
    timeout: float = _JSON_TIMEOUT,
    **kwargs: Any,
) -> tuple[int, dict[str, Any]]:
    """One control-surface request: ``(status, body)``; an error status raises."""
    response = _agent_http.call(
        method, path, json_body=kwargs.get("json"), params=kwargs.get("params"), timeout=timeout
    )
    body = _as_dict(response.body)
    if not response.ok:
        _raise_api_error(response.status, body)
    return response.status, body


def _download(path: str, dest: Path) -> int:
    """Stream a GET to `dest`, returning the bytes written.

    STREAMED, never buffered. A clip is a whole video file: reading it into
    memory to write it straight back out is how a long review empties the RAM on
    the box being reviewed, and the route it reads was written to stream for the
    same reason.
    """
    response = _agent_http.download(path, dest, timeout=_CLIP_TIMEOUT)
    if not response.ok:
        _raise_api_error(response.status, _as_dict(response.body))
    return int(response.body)


def _fmt_bytes(value: Any) -> str:
    if not isinstance(value, (int, float)):
        return "—"
    size = float(value)
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024 or unit == "GB":
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} GB"


def _fmt_mtime(value: Any) -> str:
    if not isinstance(value, (int, float)):
        return "—"
    try:
        return (
            datetime.fromtimestamp(float(value), tz=timezone.utc)
            .astimezone()
            .strftime("%Y-%m-%d %H:%M:%S")
        )
    except (OverflowError, OSError, ValueError):
        return "—"


@click.group("record", help="Start, stop, list and clip ground-station recordings.")
def record_group() -> None:
    pass


@record_group.command("start", help="Begin a capture on the ground station.")
@click.option("--hint", default=None, help="Name fragment to append to the filename.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def record_start(hint: str | None, as_json: bool) -> None:
    payload: dict[str, Any] = {}
    if hint:
        payload["filename_hint"] = hint
    _, body = _request("POST", f"{_RECORDING_BASE}/start", json=payload)
    if as_json:
        click.echo(json.dumps(body, indent=2, sort_keys=True))
        return

    theme = _ansi.detect_theme()
    click.echo(_ansi.marker(theme, "RECORDING STARTED"))
    click.echo(_ansi.kv(theme, "file", str(body.get("filename", "?"))))
    click.echo(_ansi.kv(theme, "started", str(body.get("started_at", "?"))))
    click.echo(_ansi.kv(theme, "path", str(body.get("path", "?"))))


@record_group.command("stop", help="End the in-flight capture.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def record_stop(as_json: bool) -> None:
    _, body = _request("POST", f"{_RECORDING_BASE}/stop", json={})
    if as_json:
        click.echo(json.dumps(body, indent=2, sort_keys=True))
        return

    theme = _ansi.detect_theme()
    duration = body.get("duration_seconds")
    click.echo(_ansi.marker(theme, "RECORDING STOPPED"))
    click.echo(_ansi.kv(theme, "file", str(body.get("filename", "?"))))
    click.echo(
        _ansi.kv(
            theme,
            "duration",
            _ansi.fmt_dur(float(duration)) if isinstance(duration, (int, float)) else "—",
        )
    )
    click.echo(_ansi.kv(theme, "size", _fmt_bytes(body.get("size_bytes"))))


@record_group.command(
    "list",
    help="List on-disk captures, or a stream's playback segments with --path.",
)
@click.option(
    "--path",
    "stream_path",
    default=None,
    help="Read mediamtx's playback segments for this stream instead of the disk listing.",
)
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def record_list(stream_path: str | None, as_json: bool) -> None:
    if stream_path:
        _, body = _request(
            "GET", f"{_RECORDING_BASE}/segments", params={"path": stream_path}
        )
        if as_json:
            click.echo(json.dumps(body, indent=2, sort_keys=True))
            return
        _print_segments(stream_path, body)
        return

    _, body = _request("GET", f"{_RECORDING_BASE}/list")
    if as_json:
        click.echo(json.dumps(body, indent=2, sort_keys=True))
        return
    _print_listing(body)


def _print_listing(body: dict[str, Any]) -> None:
    theme = _ansi.detect_theme()
    recording = bool(body.get("recording"))
    current = body.get("current_filename")
    items = body.get("items") or []

    click.echo(_ansi.marker(theme, "RECORDINGS"))
    live = "yes" if recording else "no"
    if recording and isinstance(current, str):
        live = f"yes  ·  {current}"
    click.echo(
        f"  {_ansi.dot(theme, 'ok' if recording else 'pending')} "
        f"{theme.dim('in flight'.ljust(11))}  {live}"
    )
    click.echo(_ansi.kv(theme, "captures", str(len(items))))
    click.echo("")

    if not items:
        click.echo(f"  {theme.dim('nothing recorded on this node yet')}")
        return
    for item in items:
        if not isinstance(item, dict):
            continue
        click.echo(
            f"  {str(item.get('filename', '?')):<44} "
            f"{_fmt_bytes(item.get('size_bytes')):>10}  "
            f"{theme.dim(_fmt_mtime(item.get('mtime')))}"
        )


def _print_segments(stream_path: str, body: dict[str, Any]) -> None:
    theme = _ansi.detect_theme()
    # mediamtx answers a JSON ARRAY, which `_as_dict` wraps under "data".
    segments = body.get("data") if isinstance(body.get("data"), list) else []

    click.echo(_ansi.marker(theme, f"SEGMENTS · {stream_path}"))
    click.echo(_ansi.kv(theme, "segments", str(len(segments))))
    click.echo("")
    if not segments:
        click.echo(f"  {theme.dim('mediamtx has no recorded segments for this path')}")
        return
    for segment in segments:
        if not isinstance(segment, dict):
            continue
        duration = segment.get("duration")
        click.echo(
            f"  {str(segment.get('start', '?')):<34} "
            f"{_ansi.fmt_dur(float(duration)) if isinstance(duration, (int, float)) else '—':>8}"
        )
    click.echo("")
    first = segments[0].get("start") if isinstance(segments[0], dict) else None
    if isinstance(first, str):
        click.echo(
            theme.dim(
                f"Next: ados record clip --path {stream_path} --start {first} "
                f"--duration 30 -o clip.mp4"
            )
        )


@record_group.command("clip", help="Cut an fMP4 clip out of the recorded segments.")
@click.option("--path", "stream_path", required=True, help="Stream path to cut from.")
@click.option("--start", required=True, help="RFC 3339 start timestamp.")
@click.option("--duration", required=True, type=float, help="Clip length in seconds.")
@click.option(
    "--output",
    "-o",
    required=True,
    type=click.Path(dir_okay=False, path_type=Path),
    help="File to write the clip to.",
)
def record_clip(stream_path: str, start: str, duration: float, output: Path) -> None:
    # The route bounds these too — it is the authority and refuses an
    # unparseable start or an absurd duration — but a local check spends no round
    # trip on a value that cannot possibly work.
    if duration <= 0:
        raise click.ClickException("--duration must be greater than 0 seconds.")
    query = {"path": stream_path, "start": start, "duration": f"{duration:g}"}
    request = httpx.QueryParams(query)
    written = _download(f"{_RECORDING_BASE}/clip?{request}", output)
    if written == 0:
        raise click.ClickException(
            "The playback server returned an empty clip; check --start against "
            f"`ados record list --path {stream_path}`."
        )
    theme = _ansi.detect_theme()
    click.echo(_ansi.marker(theme, "CLIP WRITTEN"))
    click.echo(_ansi.kv(theme, "file", str(output)))
    click.echo(_ansi.kv(theme, "size", _fmt_bytes(written)))
