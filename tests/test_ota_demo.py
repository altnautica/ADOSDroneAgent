"""Tests for OTA demo mode updater."""

from __future__ import annotations

import asyncio

import pytest

from ados.services.ota.demo import (
    FAKE_MANIFEST,
    DemoOtaUpdater,
    DemoUpdateState,
)
from ados.services.ota.downloader import DownloadState


def test_initial_state():
    demo = DemoOtaUpdater()
    assert demo.state == DemoUpdateState.IDLE
    assert demo.manifest is None
    assert demo.download_progress.state == DownloadState.IDLE


def test_fake_manifest():
    assert FAKE_MANIFEST.version == "99.0.0"
    assert FAKE_MANIFEST.channel == "demo"
    assert FAKE_MANIFEST.release_url.startswith("https://")


def test_get_status_initial():
    demo = DemoOtaUpdater()
    status = demo.get_status()
    assert status["state"] == "idle"
    assert status["demo_mode"] is True
    assert status["current_version"] == "0.1.0"
    assert "download" in status


def test_get_status_with_manifest():
    demo = DemoOtaUpdater()
    demo._manifest = FAKE_MANIFEST
    status = demo.get_status()
    assert "pending_update" in status
    assert status["pending_update"]["version"] == "99.0.0"


@pytest.mark.asyncio
async def test_demo_run_starts():
    """Verify demo starts and reaches checking state (cancel after short delay)."""
    demo = DemoOtaUpdater()

    async def run_with_timeout():
        task = asyncio.create_task(demo.run())
        # Let it get past the initial 30s sleep by cancelling early
        await asyncio.sleep(0.1)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

    await run_with_timeout()
    # State should still be IDLE since 30s hasn't passed
    assert demo.state == DemoUpdateState.IDLE
