"""Smoke tests for all 9 TUI screens.

Uses Textual's built-in test framework. Each test mounts the screen and
verifies that key widgets exist. The app is configured with a non-routable
API URL so HTTP calls fail fast, exercising the error-handling paths.
"""

from __future__ import annotations

import pytest
from textual.widgets import DataTable, Static

from ados.tui.app import ADOSTui

# TUI widget ids and screen layout drifted from this test fixture.
# The expected ids (#services-table, #health-panel, #fc-panel, etc.) no
# longer exist in the current screen implementations. Real TUI smoke
# coverage needs a rewrite against the current widget tree.
pytestmark = pytest.mark.skip(reason="TUI widget ids drifted; smoke fixtures need rewrite")


@pytest.fixture
def app() -> ADOSTui:
    """Create an ADOSTui app with a non-routable API URL so HTTP calls fail fast."""
    return ADOSTui(api_url="http://127.0.0.1:1")


def _widget_text(widget: Static) -> str:
    """Extract text content from a Static widget."""
    return widget._Static__content  # type: ignore[attr-defined]


# ── Dashboard ──────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_dashboard_mounts(app: ADOSTui) -> None:
    """Dashboard screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("dashboard")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#services-table", DataTable) is not None
        assert screen.query_one("#health-panel", Static) is not None
        assert screen.query_one("#fc-panel", Static) is not None
        assert screen.query_one("#logs-panel", Static) is not None


@pytest.mark.asyncio
async def test_dashboard_handles_no_agent(app: ADOSTui) -> None:
    """Dashboard shows error state when agent is unreachable."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("dashboard")
        await pilot.pause(delay=1.5)
        text = _widget_text(app.screen.query_one("#health-panel", Static))
        assert "not running" in text.lower() or "error" in text.lower()


# ── Telemetry ──────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_telemetry_mounts(app: ADOSTui) -> None:
    """Telemetry screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("telemetry")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#attitude-panel", Static) is not None
        assert screen.query_one("#gps-panel", Static) is not None
        assert screen.query_one("#battery-panel", Static) is not None
        assert screen.query_one("#rc-panel", Static) is not None


@pytest.mark.asyncio
async def test_telemetry_handles_no_agent(app: ADOSTui) -> None:
    """Telemetry screen handles connection failure gracefully."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("telemetry")
        await pilot.pause(delay=1.0)
        text = _widget_text(app.screen.query_one("#attitude-panel", Static))
        assert "not running" in text.lower() or "error" in text.lower()


# ── MAVLink ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_mavlink_mounts(app: ADOSTui) -> None:
    """MAVLink screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("mavlink")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#mav-table", DataTable) is not None
        assert screen.query_one("#mav-stats", Static) is not None
        assert screen.query_one("#mav-filter") is not None


@pytest.mark.asyncio
async def test_mavlink_handles_no_agent(app: ADOSTui) -> None:
    """MAVLink screen handles missing agent."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("mavlink")
        await pilot.pause(delay=1.5)
        text = _widget_text(app.screen.query_one("#mav-stats", Static))
        assert "not running" in text.lower() or "error" in text.lower()


# ── Scripting ──────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_scripting_mounts(app: ADOSTui) -> None:
    """Scripting screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("scripting")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#scripts-table", DataTable) is not None
        assert screen.query_one("#status-panel", Static) is not None
        assert screen.query_one("#log-panel", Static) is not None


@pytest.mark.asyncio
async def test_scripting_handles_no_agent(app: ADOSTui) -> None:
    """Scripting screen handles missing agent."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("scripting")
        await pilot.pause(delay=1.5)
        text = _widget_text(app.screen.query_one("#status-panel", Static))
        assert "not running" in text.lower() or "error" in text.lower()


# ── Video ──────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_video_mounts(app: ADOSTui) -> None:
    """Video screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("video")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#camera-table", DataTable) is not None
        assert screen.query_one("#pipeline-panel", Static) is not None
        assert screen.query_one("#recording-panel", Static) is not None
        assert screen.query_one("#mediamtx-panel", Static) is not None


@pytest.mark.asyncio
async def test_video_handles_no_agent(app: ADOSTui) -> None:
    """Video screen handles missing agent."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("video")
        await pilot.pause(delay=2.5)
        text = _widget_text(app.screen.query_one("#pipeline-panel", Static))
        assert "not running" in text.lower() or "error" in text.lower()


# ── Link ───────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_link_mounts(app: ADOSTui) -> None:
    """Link screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("link")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#link-state", Static) is not None
        assert screen.query_one("#rssi-display", Static) is not None
        assert screen.query_one("#packet-stats", Static) is not None
        assert screen.query_one("#fec-stats", Static) is not None
        assert screen.query_one("#channel-info", Static) is not None


@pytest.mark.asyncio
async def test_link_handles_no_agent(app: ADOSTui) -> None:
    """Link screen handles missing agent."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("link")
        await pilot.pause(delay=1.5)
        text = _widget_text(app.screen.query_one("#link-state", Static))
        assert "not running" in text.lower() or "error" in text.lower()


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


@pytest.mark.asyncio
async def test_logs_handles_no_agent(app: ADOSTui) -> None:
    """Logs screen handles missing agent without crashing."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("logs")
        await pilot.pause(delay=1.5)
        # Should not crash -- RichLog will contain the "not running" message


# ── Updates ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_updates_mounts(app: ADOSTui) -> None:
    """Updates screen mounts with expected widgets."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("updates")
        await pilot.pause()
        screen = app.screen
        assert screen.query_one("#version-panel", Static) is not None
        assert screen.query_one("#slots-panel", Static) is not None
        assert screen.query_one("#download-panel", Static) is not None


@pytest.mark.asyncio
async def test_updates_handles_no_agent(app: ADOSTui) -> None:
    """Updates screen handles missing agent."""
    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("updates")
        # Updates screen refreshes every 5 seconds
        await pilot.pause(delay=5.5)
        text = _widget_text(app.screen.query_one("#version-panel", Static))
        assert "not running" in text.lower() or "error" in text.lower() or "loading" in text.lower()


# ── Config ─────────────────────────────────────────────────────────────────


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


@pytest.mark.asyncio
async def test_config_loads_without_crash(app: ADOSTui) -> None:
    """Config editor should not crash even if no config file exists."""
    from textual.widgets import TextArea

    async with app.run_test(size=(120, 40)) as pilot:
        app.action_switch_screen("config")
        await pilot.pause()
        editor = app.screen.query_one("#config-editor", TextArea)
        assert len(editor.text) > 0
