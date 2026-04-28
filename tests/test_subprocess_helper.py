"""Unit tests for :mod:`ados.core.subprocess`."""

from __future__ import annotations

import asyncio

import pytest

from ados.core.subprocess import (
    CmdError,
    CmdResult,
    CmdTimeout,
    run_cmd,
    run_cmd_sync,
)


# ---------------------------------------------------------------------------
# Async path
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_cmd_success_returns_decoded_stdout() -> None:
    result = await run_cmd(["echo", "hello world"])
    assert isinstance(result, CmdResult)
    assert result.ok is True
    assert result.returncode == 0
    assert result.stdout.strip() == "hello world"
    assert result.stderr == ""


@pytest.mark.asyncio
async def test_run_cmd_nonzero_exit_returns_not_ok() -> None:
    # `false` always exits 1 on POSIX. No stderr expected.
    result = await run_cmd(["false"])
    assert result.ok is False
    assert result.returncode != 0


@pytest.mark.asyncio
async def test_run_cmd_check_true_raises_cmderror_on_failure() -> None:
    with pytest.raises(CmdError) as ei:
        await run_cmd(
            ["sh", "-c", "echo bad >&2; exit 7"],
            check=True,
        )
    assert ei.value.result.returncode == 7
    assert "bad" in ei.value.result.stderr


@pytest.mark.asyncio
async def test_run_cmd_timeout_raises_asyncio_timeout() -> None:
    with pytest.raises(asyncio.TimeoutError):
        await run_cmd(["sleep", "5"], timeout=0.2)


@pytest.mark.asyncio
async def test_run_cmd_stdin_input_is_forwarded() -> None:
    result = await run_cmd(
        ["cat"],
        input_text="ground station\n",
        timeout=2.0,
    )
    assert result.ok is True
    assert result.stdout == "ground station\n"


# ---------------------------------------------------------------------------
# Sync path
# ---------------------------------------------------------------------------


def test_run_cmd_sync_success() -> None:
    result = run_cmd_sync(["echo", "ok"])
    assert result.ok is True
    assert result.stdout.strip() == "ok"


def test_run_cmd_sync_nonzero_exit() -> None:
    result = run_cmd_sync(["false"])
    assert result.ok is False


def test_run_cmd_sync_check_true_raises_on_failure() -> None:
    with pytest.raises(CmdError):
        run_cmd_sync(["sh", "-c", "exit 3"], check=True)


def test_run_cmd_sync_timeout_raises_cmdtimeout() -> None:
    with pytest.raises(CmdTimeout):
        run_cmd_sync(["sleep", "5"], timeout=0.2)


def test_run_cmd_sync_stdin_input_forwarded() -> None:
    result = run_cmd_sync(["cat"], input_text="payload\n", timeout=2.0)
    assert result.ok is True
    assert result.stdout == "payload\n"
