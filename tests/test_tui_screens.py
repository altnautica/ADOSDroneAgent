"""Smoke tests for all 9 TUI screens.

Uses Textual's built-in test framework. Each test mounts the screen and
verifies that key widgets exist by id. The app is configured with a
non-routable API URL so HTTP calls fail fast, exercising the
error-handling paths.

The widget ids tracked here mirror the live `compose()` output of each
screen under `src/ados/tui/screens/`. Update both sides when a screen
gains, drops, or renames an id.
"""

from __future__ import annotations

import pytest
from textual.widgets import DataTable, Static

from ados.tui.app import ADOSTui


@pytest.fixture
def app() -> ADOSTui:
    """Create an ADOSTui app with a non-routable API URL so HTTP calls fail fast."""
    return ADOSTui(api_url="http://127.0.0.1:1")


# ── Dashboard ──────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_dashboard_mounts(app: ADOSTui) -> None:
    """Dashboard screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("dashboard")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#banner-info", Static) is not None
        assert screen.query_one("#fc-details", Static) is not None
        assert screen.query_one("#batt-details", Static) is not None
        assert screen.query_one("#gps-info", Static) is not None
        assert screen.query_one("#link-info", Static) is not None


@pytest.mark.asyncio
async def test_dashboard_handles_no_agent(app: ADOSTui) -> None:
    """Dashboard shows error state when agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("dashboard")
        await pilot.pause(delay=1.5)
        banner = app.screen.query_one("#banner-info", Static)
        rendered = str(banner.render())
        assert "not running" in rendered.lower() or "error" in rendered.lower()


# ── Telemetry ──────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_telemetry_mounts(app: ADOSTui) -> None:
    """Telemetry screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("telemetry")
        await pilot.pause()
        screen = app.screen
        # AttitudeIndicator + GaugeBar + SatelliteBar are all custom
        # subclasses of Static (or Widget) so #attitude resolves to a
        # widget; we only need to confirm presence by id.
        assert screen.query_one("#attitude") is not None
        assert screen.query_one("#telem-gps-info", Static) is not None
        assert screen.query_one("#telem-batt-info", Static) is not None
        assert screen.query_one("#rc-ch1") is not None
        assert screen.query_one("#telem-sat-bar") is not None


@pytest.mark.asyncio
async def test_telemetry_handles_no_agent(app: ADOSTui) -> None:
    """Telemetry screen shows error state when agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("telemetry")
        await pilot.pause(delay=1.0)
        attitude = app.screen.query_one("#attitude")
        rendered = str(attitude.render())
        assert "not running" in rendered.lower() or "error" in rendered.lower()


# ── MAVLink ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_mavlink_mounts(app: ADOSTui) -> None:
    """MAVLink screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("mavlink")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#mav-table", DataTable) is not None
        assert screen.query_one("#mav-rate", Static) is not None
        assert screen.query_one("#mav-total", Static) is not None
        assert screen.query_one("#mav-signing-status", Static) is not None
        assert screen.query_one("#mav-filter") is not None


@pytest.mark.asyncio
async def test_mavlink_handles_no_agent(app: ADOSTui) -> None:
    """MAVLink screen mounts cleanly even when the agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("mavlink")
        await pilot.pause(delay=1.5)
        # No explicit error banner today; just assert the screen still
        # exposes its core widgets without crashing.
        assert app.screen.query_one("#mav-table", DataTable) is not None


# ── Scripting ──────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_scripting_mounts(app: ADOSTui) -> None:
    """Scripting screen mounts with expected widgets."""
    from textual.widgets import RichLog

    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("scripting")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#scripts-table", DataTable) is not None
        assert screen.query_one("#engine-stats", Static) is not None
        assert screen.query_one("#cmd-log", RichLog) is not None


@pytest.mark.asyncio
async def test_scripting_handles_no_agent(app: ADOSTui) -> None:
    """Scripting screen mounts cleanly when agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("scripting")
        await pilot.pause(delay=1.5)
        assert app.screen.query_one("#scripts-table", DataTable) is not None


# ── Video ──────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_video_mounts(app: ADOSTui) -> None:
    """Video screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("video")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#camera-table", DataTable) is not None
        assert screen.query_one("#pipeline-detail", Static) is not None
        assert screen.query_one("#recording-detail", Static) is not None
        assert screen.query_one("#mediamtx-detail", Static) is not None
        assert screen.query_one("#roles-panel", Static) is not None


@pytest.mark.asyncio
async def test_video_handles_no_agent(app: ADOSTui) -> None:
    """Video screen mounts cleanly when agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("video")
        await pilot.pause(delay=2.5)
        assert app.screen.query_one("#camera-table", DataTable) is not None


# ── Link ───────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_link_mounts(app: ADOSTui) -> None:
    """Link screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("link")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#link-state-dot") is not None
        assert screen.query_one("#link-iface-info", Static) is not None
        assert screen.query_one("#link-rssi-gauge") is not None
        assert screen.query_one("#link-pkt-info", Static) is not None
        assert screen.query_one("#link-fec-info", Static) is not None
        assert screen.query_one("#link-chan-info", Static) is not None


@pytest.mark.asyncio
async def test_link_handles_no_agent(app: ADOSTui) -> None:
    """Link screen mounts cleanly when agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("link")
        await pilot.pause(delay=1.5)
        assert app.screen.query_one("#link-state-dot") is not None


# ── Logs ───────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_logs_mounts(app: ADOSTui) -> None:
    """Logs screen mounts with expected widgets."""
    from textual.widgets import Button, Input, RichLog

    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("logs")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#log-output", RichLog) is not None
        assert screen.query_one("#log-search", Input) is not None
        assert screen.query_one("#btn-all", Button) is not None
        assert screen.query_one("#btn-error", Button) is not None


@pytest.mark.asyncio
async def test_logs_handles_no_agent(app: ADOSTui) -> None:
    """Logs screen mounts cleanly when agent is unreachable."""
    from textual.widgets import RichLog

    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("logs")
        await pilot.pause(delay=1.5)
        assert app.screen.query_one("#log-output", RichLog) is not None


# ── Updates ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_updates_mounts(app: ADOSTui) -> None:
    """Updates screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("updates")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#version-detail", Static) is not None
        assert screen.query_one("#update-info-detail", Static) is not None
        assert screen.query_one("#download-detail", Static) is not None
        assert screen.query_one("#dl-gauge") is not None


@pytest.mark.asyncio
async def test_updates_handles_no_agent(app: ADOSTui) -> None:
    """Updates screen mounts cleanly when agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("updates")
        # Updates screen refreshes every 5 seconds; pause shorter for speed.
        await pilot.pause(delay=1.5)
        assert app.screen.query_one("#version-detail", Static) is not None


# ── Config ─────────────────────────────────────────────────────────────────
#
# The config editor uses Textual TextArea with `language="yaml"`, which
# requires the optional tree-sitter-yaml package to register the syntax
# highlighter. CI / local dev environments often skip that wheel because
# tree-sitter binaries are platform-specific. We skip the config tests
# unless that grammar is available.


def _has_yaml_grammar() -> bool:
    try:
        import tree_sitter_yaml  # noqa: F401
        return True
    except Exception:
        return False


_yaml_grammar_skip = pytest.mark.skipif(
    not _has_yaml_grammar(),
    reason="tree-sitter-yaml grammar not installed; TextArea(language='yaml') cannot mount",
)


@_yaml_grammar_skip
@pytest.mark.asyncio
async def test_config_mounts(app: ADOSTui) -> None:
    """Config screen mounts with expected widgets."""
    from textual.widgets import Button, TextArea

    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("config")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#config-editor", TextArea) is not None
        assert screen.query_one("#config-path", Static) is not None
        assert screen.query_one("#btn-save", Button) is not None
        assert screen.query_one("#btn-reload", Button) is not None


@_yaml_grammar_skip
@pytest.mark.asyncio
async def test_config_loads_without_crash(app: ADOSTui) -> None:
    """Config editor should not crash even if no config file exists."""
    from textual.widgets import TextArea

    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("config")
        await pilot.pause()
        editor = app.screen.query_one("#config-editor", TextArea)
        # Editor should exist with some content (loaded config or default placeholder).
        assert editor is not None
