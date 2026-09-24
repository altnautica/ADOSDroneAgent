"""The runner's lifecycle against a live (fake) host socket.

Drives ``runner._run`` end to end: a real plugin file on disk, a unix-socket
host that answers the handshake, and the three ways a running plugin leaves its
wait -- a stop signal, and the host dropping the connection -- plus the config
handed to ``on_configure``.
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import signal
import sys
import tempfile
import time
from pathlib import Path

import pytest

import ados.plugins.runner as runner
from ados.plugins.rpc import Envelope, encode_frame, read_frame

PLUGIN_ID = "com.example.lifecycle"

MANIFEST = f"""\
id: {PLUGIN_ID}
version: 1.0.0
name: Lifecycle
compatibility:
  ados_version: ">=0.1.0"
agent:
  entrypoint: agent/plugin.py
"""

PLUGIN_SOURCE = """\
import json
import os


def _record(entry):
    with open(os.environ["LIFECYCLE_RECORD"], "a") as fh:
        fh.write(json.dumps(entry) + "\\n")


class Plugin:
    async def on_configure(self, ctx, config):
        _record({"hook": "on_configure", "config": config})

    async def on_start(self, ctx):
        _record({"hook": "on_start"})

    async def on_stop(self, ctx):
        _record({"hook": "on_stop"})
"""


class _Host:
    """Answers every request with an empty success; can drop the client."""

    def __init__(self) -> None:
        self.writer: asyncio.StreamWriter | None = None
        self.server: asyncio.AbstractServer | None = None

    async def start(self, path: Path) -> None:
        self.server = await asyncio.start_unix_server(self._serve, str(path))

    async def _serve(self, reader, writer) -> None:
        self.writer = writer
        while True:
            try:
                env = await read_frame(reader)
            except Exception:
                return
            if env is None:
                return
            writer.write(
                encode_frame(
                    Envelope(
                        type="response",
                        method=env.method,
                        capability="",
                        args={},
                        request_id=env.request_id,
                        token="",
                    )
                )
            )
            await writer.drain()

    def drop_client(self) -> None:
        assert self.writer is not None
        self.writer.close()

    async def stop(self) -> None:
        if self.writer is not None:
            self.writer.close()
        if self.server is not None:
            self.server.close()


@pytest.fixture
def plugin_env(tmp_path, monkeypatch):
    install = tmp_path / "plugins"
    plugin_dir = install / PLUGIN_ID
    (plugin_dir / "agent").mkdir(parents=True)
    (plugin_dir / "manifest.yaml").write_text(MANIFEST)
    (plugin_dir / "agent" / "plugin.py").write_text(PLUGIN_SOURCE)
    data = tmp_path / "plugin-data"
    (data / PLUGIN_ID).mkdir(parents=True)
    (data / PLUGIN_ID / "config.yaml").write_text("mode: fast\n")
    record = tmp_path / "record.jsonl"
    monkeypatch.setattr(runner, "PLUGINS_INSTALL_DIR", install)
    monkeypatch.setattr(runner, "PLUGIN_DATA_DIR", data)
    monkeypatch.setattr(tempfile, "tempdir", str(tmp_path / "tmp"))
    monkeypatch.delenv("ADOS_PLUGIN_DATA_DIR", raising=False)
    monkeypatch.setenv("LIFECYCLE_RECORD", str(record))
    sock_dir = Path(tempfile.mkdtemp(prefix="adr", dir="/tmp"))
    try:
        yield sock_dir / "h.sock", record
    finally:
        shutil.rmtree(sock_dir, ignore_errors=True)


def _hooks(record: Path) -> list[dict]:
    if not record.exists():
        return []
    return [json.loads(line) for line in record.read_text().splitlines()]


async def _wait_for_hook(record: Path, hook: str) -> None:
    for _ in range(200):
        if any(h["hook"] == hook for h in _hooks(record)):
            return
        await asyncio.sleep(0.01)
    raise AssertionError(f"{hook} never ran: {_hooks(record)}")


@pytest.mark.asyncio
async def test_a_dropped_host_connection_ends_the_runner_for_a_restart(plugin_env):
    """A plugin host restart must not leave the plugin up and inert.

    The runner has to notice the closed bridge, give the plugin its on_stop,
    and exit non-zero so systemd restarts it into a fresh connection.
    """
    sock, record = plugin_env
    host = _Host()
    await host.start(sock)
    try:
        task = asyncio.create_task(
            runner._run(PLUGIN_ID, socket_path=str(sock), capability_token="t", agent_id="")
        )
        await _wait_for_hook(record, "on_start")
        host.drop_client()
        code = await asyncio.wait_for(task, timeout=2.0)
    finally:
        await host.stop()
    assert code == 4
    assert [h["hook"] for h in _hooks(record)][-1] == "on_stop"


@pytest.mark.asyncio
async def test_on_configure_receives_the_static_config(plugin_env):
    sock, record = plugin_env
    host = _Host()
    await host.start(sock)
    try:
        task = asyncio.create_task(
            runner._run(PLUGIN_ID, socket_path=str(sock), capability_token="t", agent_id="")
        )
        await _wait_for_hook(record, "on_start")
        host.drop_client()
        await asyncio.wait_for(task, timeout=2.0)
    finally:
        await host.stop()
    configured = [h for h in _hooks(record) if h["hook"] == "on_configure"]
    assert configured == [{"hook": "on_configure", "config": {"mode": "fast"}}]


_RUNNER_SCRIPT = """\
import asyncio, os, sys, tempfile
from pathlib import Path
import ados.plugins.runner as runner
runner.PLUGINS_INSTALL_DIR = Path(os.environ["T_INSTALL"])
runner.PLUGIN_DATA_DIR = Path(os.environ["T_DATA"])
tempfile.tempdir = os.environ["T_TMP"]
sys.exit(asyncio.run(runner._run(
    os.environ["T_ID"], socket_path=os.environ["T_SOCK"],
    capability_token="t", agent_id="",
)))
"""


@pytest.mark.asyncio
async def test_sigterm_stops_an_idle_plugin_promptly(plugin_env, tmp_path):
    """SIGTERM must wake the runner even when nothing else is happening.

    Run as its own process under ``asyncio.run``, exactly as the unit runs it:
    a signal handler that only sets an event does not wake a loop parked in
    select, so the stop would land at systemd's kill timeout with on_stop
    never run.
    """
    sock, record = plugin_env
    host = _Host()
    await host.start(sock)
    env = {
        **os.environ,
        "T_INSTALL": str(runner.PLUGINS_INSTALL_DIR),
        "T_DATA": str(runner.PLUGIN_DATA_DIR),
        "T_TMP": str(tmp_path / "tmp"),
        "T_ID": PLUGIN_ID,
        "T_SOCK": str(sock),
    }
    proc = await asyncio.create_subprocess_exec(
        sys.executable, "-c", _RUNNER_SCRIPT, env=env
    )
    try:
        for _ in range(1000):
            if any(h["hook"] == "on_start" for h in _hooks(record)):
                break
            await asyncio.sleep(0.01)
        else:
            raise AssertionError("runner never started the plugin")
        started = time.monotonic()
        proc.send_signal(signal.SIGTERM)
        code = await asyncio.wait_for(proc.wait(), timeout=3.0)
        elapsed = time.monotonic() - started
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
        await host.stop()
    assert code == 0
    assert elapsed < 1.0, f"SIGTERM took {elapsed:.2f}s to reach the runner"
    assert [h["hook"] for h in _hooks(record)][-1] == "on_stop"
