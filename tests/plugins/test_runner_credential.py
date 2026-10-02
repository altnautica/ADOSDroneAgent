"""The runner takes its launch values from the systemd token credential.

The plugin unit loads the host-written ``KEY=VALUE`` file with
``LoadCredential=``; the token is never in the process environment. A runner
that still looked only at the environment would wait for a token forever.
"""

from __future__ import annotations

import asyncio
import shutil
import tempfile
from pathlib import Path

import pytest

import ados.plugins.runner as runner
from ados.plugins.rpc import Envelope, encode_frame, read_frame

PLUGIN_ID = "com.example.credential"


class _RecordingHost:
    """Answers the hello and records the token it carried."""

    def __init__(self) -> None:
        self.tokens: list[str] = []
        self.server: asyncio.AbstractServer | None = None

    async def start(self, path: Path) -> None:
        self.server = await asyncio.start_unix_server(self._serve, str(path))

    async def _serve(self, reader, writer) -> None:
        env = await read_frame(reader)
        if env is None:
            return
        self.tokens.append(env.token)
        writer.write(
            encode_frame(
                Envelope(
                    type="response",
                    method=env.method,
                    capability="",
                    args={"ready": True},
                    request_id=env.request_id,
                    token="",
                )
            )
        )
        await writer.drain()

    async def stop(self) -> None:
        if self.server is not None:
            self.server.close()


@pytest.fixture
def sock_path():
    sock_dir = Path(tempfile.mkdtemp(prefix="adc", dir="/tmp"))
    try:
        yield sock_dir / "h.sock"
    finally:
        shutil.rmtree(sock_dir, ignore_errors=True)


def _write_credential(directory: Path, body: str) -> None:
    directory.mkdir()
    (directory / "ados-plugin-token").write_text(body, encoding="utf-8")


@pytest.mark.asyncio
async def test_the_bridge_presents_the_credential_token(tmp_path, monkeypatch, sock_path):
    creds = tmp_path / "creds"
    # A token is base64 text joined by `|` and may carry `=` padding; it is
    # taken verbatim after the first `=`.
    _write_credential(creds, "ADOS_PLUGIN_TOKEN=pay=load|sig==\nADOS_PLUGIN_AGENT_ID=\n")
    monkeypatch.setenv("CREDENTIALS_DIRECTORY", str(creds))
    monkeypatch.setenv("ADOS_PLUGIN_TOKEN", "stale-from-env")
    host = _RecordingHost()
    await host.start(sock_path)
    try:
        client = await asyncio.wait_for(
            runner._await_bridge(PLUGIN_ID, str(sock_path), None), timeout=2.0
        )
        await client.close()
    finally:
        await host.stop()
    assert host.tokens == ["pay=load|sig=="]


def test_launch_values_fall_back_to_the_environment_without_a_credential(monkeypatch):
    # The launchd entry script exports the credential's keys instead.
    monkeypatch.delenv("CREDENTIALS_DIRECTORY", raising=False)
    monkeypatch.setenv("ADOS_PLUGIN_TOKEN", "from-env")
    monkeypatch.delenv("ADOS_PLUGIN_SOCKET", raising=False)
    assert runner._launch_value("ADOS_PLUGIN_TOKEN") == "from-env"
    assert runner._launch_value("ADOS_PLUGIN_SOCKET") is None


def test_an_unreadable_credential_reads_as_empty(tmp_path, monkeypatch):
    monkeypatch.setenv("CREDENTIALS_DIRECTORY", str(tmp_path / "missing"))
    monkeypatch.delenv("ADOS_PLUGIN_TOKEN", raising=False)
    assert runner._launch_value("ADOS_PLUGIN_TOKEN") is None
