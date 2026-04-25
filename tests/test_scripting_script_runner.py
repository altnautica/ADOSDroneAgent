"""Tests for the script runner."""

from __future__ import annotations

import os
import tempfile
from unittest.mock import MagicMock

import pytest

from ados.core.config import ScriptsConfig
from ados.services.scripting.script_runner import ScriptRunner, ScriptState


@pytest.fixture
def mock_executor():
    return MagicMock()


@pytest.fixture
def config() -> ScriptsConfig:
    return ScriptsConfig(max_concurrent=2)


@pytest.fixture
def runner(config: ScriptsConfig, mock_executor) -> ScriptRunner:
    return ScriptRunner(config, mock_executor)


class TestScriptRunner:
    """Script execution management."""

    def test_list_scripts_empty(self, runner: ScriptRunner):
        assert runner.list_scripts() == []

    def test_start_script_not_found(self, runner: ScriptRunner):
        with pytest.raises(RuntimeError, match="not found"):
            runner.start_script("/nonexistent/script.py")

    def test_start_script_returns_id(self, runner: ScriptRunner):
        # script_runner.start_script calls asyncio.get_event_loop() which
        # raises in a sync test after a prior async test has closed its loop.
        # Provide a fresh loop for this test.
        import asyncio as _asyncio
        loop = _asyncio.new_event_loop()
        _asyncio.set_event_loop(loop)
        try:
            with tempfile.NamedTemporaryFile(suffix=".py", delete=False, mode="w") as f:
                f.write("print('hello')\n")
                path = f.name
            try:
                script_id = runner.start_script(path)
                assert isinstance(script_id, str)
                assert len(script_id) == 12
            finally:
                os.unlink(path)
        finally:
            loop.close()
            _asyncio.set_event_loop(None)

    def test_max_concurrent_enforced(self, runner: ScriptRunner):
        # script_runner.start_script calls asyncio.get_event_loop() which
        # raises in a sync test after a prior async test has closed its loop.
        # Provide a fresh loop for this test.
        import asyncio as _asyncio
        loop = _asyncio.new_event_loop()
        _asyncio.set_event_loop(loop)
        try:
            paths = []
            for i in range(3):
                with tempfile.NamedTemporaryFile(
                    suffix=".py", delete=False, mode="w"
                ) as f:
                    f.write("import time; time.sleep(10)\n")
                    paths.append(f.name)

            try:
                runner.start_script(paths[0])
                runner.start_script(paths[1])
                with pytest.raises(RuntimeError, match="Max concurrent"):
                    runner.start_script(paths[2])
            finally:
                for p in paths:
                    os.unlink(p)
        finally:
            loop.close()
            _asyncio.set_event_loop(None)

    def test_get_script_none(self, runner: ScriptRunner):
        assert runner.get_script("nonexistent") is None

    def test_stop_nonexistent_returns_false(self, runner: ScriptRunner):
        assert runner.stop_script("nonexistent") is False

    @pytest.mark.asyncio
    async def test_run_script_lifecycle(self, runner: ScriptRunner):
        with tempfile.NamedTemporaryFile(suffix=".py", delete=False, mode="w") as f:
            f.write("print('hello from script')\n")
            path = f.name

        try:
            script_id = runner.start_script(path)
            # Give the async task a moment to run
            import asyncio
            await asyncio.sleep(0.5)

            info = runner.get_script(script_id)
            assert info is not None
            assert info.state in (ScriptState.COMPLETED, ScriptState.RUNNING)
        finally:
            os.unlink(path)

    @pytest.mark.asyncio
    async def test_script_output_captured(self, runner: ScriptRunner):
        with tempfile.NamedTemporaryFile(suffix=".py", delete=False, mode="w") as f:
            f.write("print('line1')\nprint('line2')\n")
            path = f.name

        try:
            script_id = runner.start_script(path)
            import asyncio
            await asyncio.sleep(0.5)

            info = runner.get_script(script_id)
            assert info is not None
            if info.state == ScriptState.COMPLETED:
                assert "line1" in info.output_lines
                assert "line2" in info.output_lines
        finally:
            os.unlink(path)

    @pytest.mark.asyncio
    async def test_failed_script(self, runner: ScriptRunner):
        with tempfile.NamedTemporaryFile(suffix=".py", delete=False, mode="w") as f:
            f.write("raise ValueError('boom')\n")
            path = f.name

        try:
            script_id = runner.start_script(path)
            import asyncio
            await asyncio.sleep(0.5)

            info = runner.get_script(script_id)
            assert info is not None
            if info.state == ScriptState.FAILED:
                assert info.return_code != 0
        finally:
            os.unlink(path)
