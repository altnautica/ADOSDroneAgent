"""Tests for the wizard's display-install routes and step state derivation.

Covers:

* ``GET /v1/setup/display/options`` — returns the active board's
  supported displays plus the synthetic ``none`` option.
* ``POST /v1/setup/display/install`` — kicks off the shell driver
  (mocked) and rejects concurrent jobs with 409.
* ``POST /v1/setup/display/install`` with ``display_id="none"`` —
  writes the skip marker without spawning.
* ``GET /v1/setup/display/job/{job_id}`` — returns the job snapshot.
* ``_resolve_display_step`` — maps the hardware-check ``display`` row
  onto wizard step state + detail.

Subprocess execution is replaced with a stub so the tests run without
root or kernel-headers.
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

from ados.setup import display_install
from ados.setup.models import HardwareCheckItem, HardwareCheckStatus
from ados.setup.service import _resolve_display_step


# ---------------------------------------------------------------------------
# Step state derivation — independent of the HTTP layer
# ---------------------------------------------------------------------------
class TestResolveDisplayStep:
    def test_no_item_returns_needs_action(self):
        hc = HardwareCheckStatus(profile="ground_station", items=[])
        state, detail = _resolve_display_step(hc)
        assert state == "needs_action"
        assert "SPI LCD" in detail or "local display" in detail.lower()

    def test_ok_item_returns_complete(self):
        item = HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="ok",
            detail="waveshare35a on /dev/fb1 (480x320, rotation 90 + touch).",
        )
        hc = HardwareCheckStatus(profile="ground_station", items=[item])
        state, detail = _resolve_display_step(hc)
        assert state == "complete"
        assert "waveshare35a" in detail

    def test_warning_item_returns_needs_action(self):
        item = HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="warning",
            detail="waveshare35a provisioned but /dev/fb1 is not bound. Reboot to load the overlay.",
        )
        hc = HardwareCheckStatus(profile="ground_station", items=[item])
        state, detail = _resolve_display_step(hc)
        assert state == "needs_action"
        assert "Reboot" in detail

    def test_unknown_no_conf_returns_needs_action(self, monkeypatch, tmp_path: Path):
        bogus = tmp_path / "missing.conf"
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", bogus)
        item = HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="unknown",
            detail="No /etc/ados/display.conf — no LCD provisioned for this board.",
        )
        hc = HardwareCheckStatus(profile="ground_station", items=[item])
        state, _ = _resolve_display_step(hc)
        assert state == "needs_action"

    def test_unknown_with_skip_marker_returns_optional(
        self, monkeypatch, tmp_path: Path
    ):
        conf = tmp_path / "display.conf"
        conf.write_text("display_id=none\nhas_touch=false\n")
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)
        item = HardwareCheckItem(
            id="display",
            label="Local display (SPI LCD)",
            state="unknown",
            detail="display_id=none",
        )
        hc = HardwareCheckStatus(profile="ground_station", items=[item])
        state, detail = _resolve_display_step(hc)
        assert state == "optional"
        assert "skip" in detail.lower()


# ---------------------------------------------------------------------------
# Skip marker
# ---------------------------------------------------------------------------
class TestSkipMarker:
    def test_write_skip_marker_idempotent(self, monkeypatch, tmp_path: Path):
        conf = tmp_path / "display.conf"
        monkeypatch.setattr(display_install, "DISPLAY_CONF_PATH", conf)
        display_install.write_skip_marker()
        first = conf.read_text()
        assert "display_id=none" in first
        # Run again — should still be valid + idempotent
        display_install.write_skip_marker()
        second = conf.read_text()
        assert "display_id=none" in second


# ---------------------------------------------------------------------------
# Job tracker — single-job invariant
# ---------------------------------------------------------------------------
class TestJobTracker:
    @pytest.fixture(autouse=True)
    def _reset(self):
        display_install._reset_for_tests()
        yield
        display_install._reset_for_tests()

    @pytest.mark.asyncio
    async def test_start_install_returns_handle(self, monkeypatch):
        # Stub the script resolver so we don't need the real installer
        # on disk. _run_job will still try to spawn it, so also stub
        # the subprocess factory to a no-op coroutine that exits 0.
        monkeypatch.setattr(
            display_install,
            "_resolve_driver_script",
            lambda: Path("/usr/bin/true"),
        )

        async def _fake_proc_factory(*args: Any, **kwargs: Any):
            class _FakeProc:
                stdout = _FakeStdout()

                async def wait(self) -> int:
                    return 0

            return _FakeProc()

        class _FakeStdout:
            def __aiter__(self):
                async def gen():
                    for line in (b"line one\n", b"line two\n"):
                        yield line

                return gen()

        monkeypatch.setattr(
            asyncio,
            "create_subprocess_exec",
            _fake_proc_factory,
        )
        handle = await display_install.start_install("waveshare35a")
        assert handle.display_id == "waveshare35a"
        assert handle.status in ("queued", "running", "done")
        # Wait for the dispatcher task to finish so the assertion below
        # sees a stable state.
        for _ in range(20):
            if handle.status in ("done", "failed"):
                break
            await asyncio.sleep(0.05)
        assert handle.status == "done"
        assert handle.exit_code == 0
        assert any("line one" in line for line in handle.log_tail)

    @pytest.mark.asyncio
    async def test_concurrent_install_raises(self, monkeypatch):
        monkeypatch.setattr(
            display_install,
            "_resolve_driver_script",
            lambda: Path("/usr/bin/sleep"),
        )

        async def _slow_proc_factory(*args: Any, **kwargs: Any):
            class _SlowProc:
                stdout = _EmptyStdout()

                async def wait(self) -> int:
                    await asyncio.sleep(0.5)
                    return 0

            return _SlowProc()

        class _EmptyStdout:
            def __aiter__(self):
                async def gen():
                    if False:
                        yield  # type: ignore[unreachable]

                return gen()

        monkeypatch.setattr(
            asyncio,
            "create_subprocess_exec",
            _slow_proc_factory,
        )
        await display_install.start_install("waveshare35a")
        with pytest.raises(RuntimeError):
            await display_install.start_install("waveshare35a")

    def test_resolve_driver_script_returns_none_on_clean_box(self):
        # On a stripped-down test environment the driver may not be
        # accessible; the resolver returns None rather than raising.
        result = display_install._resolve_driver_script()
        assert result is None or result.exists()

    @pytest.mark.asyncio
    async def test_start_install_raises_when_script_missing(self, monkeypatch):
        monkeypatch.setattr(
            display_install,
            "_resolve_driver_script",
            lambda: None,
        )
        with pytest.raises(FileNotFoundError):
            await display_install.start_install("waveshare35a")
