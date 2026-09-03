"""Tests for the `ados record` verb tree.

Covers what the four verbs contract on: the route each one drives, the
rendering an operator over SSH reads, the profile-mismatch diagnosis (these
routes only exist on a ground station, so running them on a drone must say
"wrong box" rather than print a raw error body), and the streamed clip write.
"""

from __future__ import annotations

import json
from unittest.mock import patch

import click
import httpx
import pytest
from click.testing import CliRunner

from ados.cli.record import record_group


def _invoke(args: list[str], responses: dict[str, tuple[int, dict]]):
    """Run `ados record <args>` with `_request` answering from `responses`.

    Keyed on the request path so a test states which route it expects the verb to
    drive; an unexpected path fails loudly instead of falling through to a
    default.
    """
    seen: list[tuple[str, str, dict]] = []

    def fake_request(method: str, path: str, **kwargs):
        seen.append((method, path, kwargs))
        if path not in responses:
            raise AssertionError(f"unexpected route {method} {path}")
        return responses[path]

    with patch("ados.cli.record._request", side_effect=fake_request):
        result = CliRunner().invoke(record_group, args)
    return result, seen


def test_record_start_posts_the_start_route_and_reports_the_filename():
    result, seen = _invoke(
        ["start", "--hint", "pipe-run"],
        {
            "/api/v1/ground-station/recording/start": (
                200,
                {
                    "filename": "2026-09-03T12-00-00_pipe-run.mp4",
                    "started_at": "2026-09-03T12:00:00+00:00",
                    "path": "/var/ados/recordings/2026-09-03T12-00-00_pipe-run.mp4",
                },
            )
        },
    )
    assert result.exit_code == 0, result.output
    method, path, kwargs = seen[0]
    assert (method, path) == ("POST", "/api/v1/ground-station/recording/start")
    assert kwargs["json"] == {"filename_hint": "pipe-run"}
    assert "2026-09-03T12-00-00_pipe-run.mp4" in result.output


def test_record_start_without_a_hint_sends_an_empty_body():
    # The hint is optional; an absent one must not become a null field the route
    # then has to interpret.
    _, seen = _invoke(
        ["start"],
        {"/api/v1/ground-station/recording/start": (200, {"filename": "a.mp4"})},
    )
    assert seen[0][2]["json"] == {}


def test_record_stop_reports_the_duration_and_size():
    result, seen = _invoke(
        ["stop"],
        {
            "/api/v1/ground-station/recording/stop": (
                200,
                {
                    "filename": "flight.mp4",
                    "stopped_at": "2026-09-03T12:01:30+00:00",
                    "duration_seconds": 90.0,
                    "size_bytes": 12 * 1024 * 1024,
                },
            )
        },
    )
    assert result.exit_code == 0, result.output
    assert seen[0][:2] == ("POST", "/api/v1/ground-station/recording/stop")
    assert "flight.mp4" in result.output
    assert "1:30" in result.output, "90 seconds renders as a duration, not raw seconds"
    assert "12.0 MB" in result.output


def test_record_list_reports_the_live_flag_and_the_captures():
    result, _ = _invoke(
        ["list"],
        {
            "/api/v1/ground-station/recording/list": (
                200,
                {
                    "recording": True,
                    "current_filename": "live.mp4",
                    "items": [
                        {"filename": "live.mp4", "size_bytes": 2048, "mtime": 1.0e9},
                        {"filename": "old.mp4", "size_bytes": 1024, "mtime": 9.9e8},
                    ],
                },
            )
        },
    )
    assert result.exit_code == 0, result.output
    assert "in flight" in result.output
    assert "yes" in result.output
    assert "live.mp4" in result.output
    assert "old.mp4" in result.output


def test_record_list_says_so_when_nothing_is_in_flight():
    # The listing envelope's recording flag is read off the live recorder, so an
    # idle node must render "no" — not a blank the operator has to interpret.
    result, _ = _invoke(
        ["list"],
        {
            "/api/v1/ground-station/recording/list": (
                200,
                {"recording": False, "current_filename": None, "items": []},
            )
        },
    )
    assert result.exit_code == 0, result.output
    assert "in flight" in result.output
    assert "no" in result.output
    assert "nothing recorded on this node yet" in result.output


def test_record_list_with_a_path_reads_the_playback_segments_instead():
    result, seen = _invoke(
        ["list", "--path", "main"],
        {
            "/api/v1/ground-station/recording/segments": (
                200,
                {
                    "data": [
                        {"start": "2026-09-03T12:00:00Z", "duration": 60.0},
                        {"start": "2026-09-03T12:01:00Z", "duration": 60.0},
                    ]
                },
            )
        },
    )
    assert result.exit_code == 0, result.output
    method, path, kwargs = seen[0]
    assert (method, path) == ("GET", "/api/v1/ground-station/recording/segments")
    assert kwargs["params"] == {"path": "main"}
    assert "2026-09-03T12:00:00Z" in result.output
    # And it hands the operator the clip command for the first segment.
    assert "ados record clip --path main" in result.output


def test_record_list_json_is_the_route_body_verbatim():
    body = {"recording": False, "current_filename": None, "items": []}
    result, _ = _invoke(
        ["list", "--json"],
        {"/api/v1/ground-station/recording/list": (200, body)},
    )
    assert result.exit_code == 0, result.output
    assert json.loads(result.output) == body


def test_a_profile_mismatch_is_reported_as_the_wrong_box_not_a_raw_body():
    # These routes are ground-station-gated. An operator who SSH'd into the drone
    # needs the diagnosis, not `{"error": {"code": "E_PROFILE_MISMATCH"}}`.
    from ados.cli.record import _raise_api_error

    with pytest.raises(click.ClickException) as excinfo:
        _raise_api_error(404, {"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    assert "not a ground station" in str(excinfo.value)


def test_an_unreachable_playback_server_points_at_the_video_diagnostic():
    from ados.cli.record import _raise_api_error

    with pytest.raises(click.ClickException) as excinfo:
        _raise_api_error(
            503,
            {
                "detail": {
                    "error": {
                        "code": "E_PLAYBACK_UNAVAILABLE",
                        "message": "not answering on loopback",
                    }
                }
            },
        )
    assert "ados diag video" in str(excinfo.value)


def test_a_recorder_error_carries_its_code_and_message():
    from ados.cli.record import _raise_api_error

    with pytest.raises(click.ClickException) as excinfo:
        _raise_api_error(
            409,
            {
                "detail": {
                    "error": {
                        "code": "E_RECORDING_ACTIVE",
                        "message": "a recording is already in progress",
                    }
                }
            },
        )
    message = str(excinfo.value)
    assert "E_RECORDING_ACTIVE" in message
    assert "already in progress" in message


def test_record_clip_streams_the_body_to_the_output_file(tmp_path):
    # The clip is written from the streamed response, so the CLI never holds a
    # whole video in memory. httpx's MockTransport drives the real `_download`.
    captured: dict[str, str] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured["url"] = str(request.url)
        return httpx.Response(
            200, content=b"ftypiso5" * 16, headers={"Content-Type": "video/mp4"}
        )

    dest = tmp_path / "clip.mp4"
    transport = httpx.MockTransport(handler)
    real_client = httpx.Client

    def client_factory(*args, **kwargs):
        kwargs["transport"] = transport
        return real_client(*args, **kwargs)

    with patch("ados.cli.record.httpx.Client", side_effect=client_factory):
        result = CliRunner().invoke(
            record_group,
            [
                "clip",
                "--path",
                "main",
                "--start",
                "2026-09-03T12:00:00Z",
                "--duration",
                "30",
                "-o",
                str(dest),
            ],
        )

    assert result.exit_code == 0, result.output
    assert dest.read_bytes() == b"ftypiso5" * 16
    assert "128 B" in result.output
    # The three parameters reach the route, with the timestamp's `:` encoded so a
    # query decode cannot mangle it.
    assert "/api/v1/ground-station/recording/clip?" in captured["url"]
    assert "path=main" in captured["url"]
    assert "duration=30" in captured["url"]
    assert "2026-09-03T12%3A00%3A00Z" in captured["url"]


def test_record_clip_refuses_a_non_positive_duration_without_a_round_trip():
    # The route bounds this too, but a value that cannot possibly work should not
    # cost a request on a link that may be the thing under test.
    called: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        called.append(str(request.url))
        return httpx.Response(200, content=b"")

    transport = httpx.MockTransport(handler)
    real_client = httpx.Client

    def client_factory(*args, **kwargs):
        kwargs["transport"] = transport
        return real_client(*args, **kwargs)

    with patch("ados.cli.record.httpx.Client", side_effect=client_factory):
        result = CliRunner().invoke(
            record_group,
            [
                "clip",
                "--path",
                "main",
                "--start",
                "2026-09-03T12:00:00Z",
                "--duration",
                "0",
                "-o",
                "unused.mp4",
            ],
        )
    assert result.exit_code != 0
    assert "greater than 0" in result.output
    assert called == [], "a doomed duration must not reach the agent"


def test_record_is_wired_into_the_top_level_cli():
    # The verbs are only reachable over SSH if the group is registered; an
    # unregistered module imports fine and does nothing.
    from ados.cli.main import cli

    assert "record" in cli.commands
