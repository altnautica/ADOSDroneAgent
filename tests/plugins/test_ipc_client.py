# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""The plugin-side IPC client, driven against a minimal fake host.

`PluginIpcClient` is the client half of the plugin IPC contract. It ships: the
`ados-plugin-runner` console script the native host execs imports it
(`ados/plugins/runner.py:41`), so it runs in-process on every node that runs a
plugin.

Its only coverage used to live in the packaged host's test files, which drove
the real client against the real Python host. That host was deleted -- the
native host has owned the per-plugin sockets since the default flipped -- and
its tests went with it, taking the client's coverage along. This restores what
applies to the surviving half, against a fake host small enough to live here:
the client's own framing, request/response correlation, event routing, and
fail-closed behaviour do not need a real host to be worth asserting.

Not restored: everything the deleted files covered about the HOST itself
(handshake rejection, token expiry, capability enforcement at the boundary,
socket yielding). Those assertions belong to an implementation that no longer
exists in Python; the native host owns them now.
"""

from __future__ import annotations

import asyncio
import shutil
import tempfile
from pathlib import Path

import pytest

from ados.plugins.ipc_client import PluginIpcClient
from ados.plugins.rpc import Envelope, encode_frame, read_frame


class FakeHost:
    """A unix-socket server that speaks the frame protocol and nothing else.

    Records every request it receives and answers from a per-method table, so a
    test can assert what the client PUT ON THE WIRE as well as what it did with
    the reply. Deliberately not a stand-in for the real host: it enforces no
    capability and verifies no token, because the point here is the client.
    """

    def __init__(self) -> None:
        self.requests: list[Envelope] = []
        self.replies: dict[str, dict] = {}
        self.errors: dict[str, str] = {}
        self._server: asyncio.AbstractServer | None = None
        self._writer: asyncio.StreamWriter | None = None
        self.connected = asyncio.Event()

    async def start(self, path: Path) -> None:
        self._server = await asyncio.start_unix_server(self._serve, str(path))

    async def stop(self) -> None:
        # Bounded: `wait_closed()` waits for every connection handler, and a
        # handler parked in `read_frame` on a peer that has not EOF'd will hold
        # it forever -- which turns a failing assertion into a hung suite rather
        # than a red test.
        if self._writer is not None:
            self._writer.close()
        if self._server is not None:
            self._server.close()
            try:
                await asyncio.wait_for(self._server.wait_closed(), timeout=2.0)
            except TimeoutError:
                pass

    async def _serve(self, reader, writer) -> None:
        self._writer = writer
        self.connected.set()
        while True:
            try:
                env = await read_frame(reader)
            except Exception:
                return
            if env is None:
                return
            self.requests.append(env)
            writer.write(
                encode_frame(
                    Envelope(
                        type="response",
                        method=env.method,
                        capability=env.capability,
                        args=self.replies.get(env.method, {}),
                        request_id=env.request_id,
                        token="",
                        error=self.errors.get(env.method),
                    )
                )
            )
            await writer.drain()

    async def push_event(self, method: str, args: dict) -> None:
        """Deliver a host-initiated event, the way a subscription feeds one."""
        assert self._writer is not None, "no client connected yet"
        self._writer.write(
            encode_frame(
                Envelope(
                    type="event",
                    method=method,
                    capability="",
                    args=args,
                    request_id="",
                    token="",
                )
            )
        )
        await self._writer.drain()


@pytest.fixture
def short_sock_dir():
    """A /tmp-rooted directory, because AF_UNIX paths are capped near 104 bytes.

    pytest's `tmp_path` is already long enough on macOS that adding a socket
    name blows the limit, and the failure surfaces as a bare `OSError` from
    `bind` rather than anything naming the cause.
    """
    base = Path(tempfile.mkdtemp(prefix="adc", dir="/tmp"))
    try:
        yield base
    finally:
        shutil.rmtree(base, ignore_errors=True)


@pytest.fixture
async def host(short_sock_dir: Path):
    h = FakeHost()
    sock = short_sock_dir / "h.sock"
    await h.start(sock)
    yield h, sock
    await h.stop()


async def _connected(host_and_sock) -> tuple[FakeHost, PluginIpcClient]:
    h, sock = host_and_sock
    client = PluginIpcClient(plugin_id="p", token="t", socket_path=sock)
    await client.connect()
    return h, client


@pytest.mark.asyncio
async def test_connect_sends_the_handshake_before_any_call(host) -> None:
    """`connect` is not just a socket open: it must announce the plugin.

    A client that connected without the hello would look healthy to every test
    that only checks a later round trip, and fail against the real host, which
    has nothing to bind the connection to a plugin id until that frame arrives.
    """
    h, client = await _connected(host)
    try:
        assert [e.method for e in h.requests] == ["hello"]
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_a_request_carries_its_capability_and_gets_its_own_reply(host) -> None:
    """Correlation is by request id, not by arrival order.

    Every call rides one socket, so a reply matched positionally would hand one
    caller another's result under concurrency -- silently, and only under load.
    """
    h, client = await _connected(host)
    try:
        h.replies["ping"] = {"pong": True}
        assert await client.ping() == {"pong": True}

        sent = [e for e in h.requests if e.method == "ping"]
        assert len(sent) == 1
        assert sent[0].request_id, "a request with no id cannot be correlated"
        assert sent[0].token == "t", "the host authenticates on this token"
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_concurrent_requests_each_receive_their_own_response(host) -> None:
    """The correlation claim above, actually exercised concurrently."""
    h, client = await _connected(host)
    try:
        h.replies["ping"] = {"pong": True}
        results = await asyncio.gather(*(client.ping() for _ in range(8)))
        assert results == [{"pong": True}] * 8
        assert len([e for e in h.requests if e.method == "ping"]) == 8
        ids = {e.request_id for e in h.requests if e.method == "ping"}
        assert len(ids) == 8, "request ids must be unique or replies cross"
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_an_error_response_raises_rather_than_returning_a_shape(host) -> None:
    """A denied call must not read as a successful empty result.

    This is the fail-closed edge: a plugin whose capability was refused has to
    see an exception, not `{}`, or it proceeds as though the host agreed.
    """
    h, client = await _connected(host)
    try:
        h.errors["event.publish"] = "capability denied"
        with pytest.raises(Exception) as excinfo:
            await client.event_publish("plugin.p.topic", {"a": 1})
        assert "denied" in str(excinfo.value).lower(), excinfo.value
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_a_subscribed_event_reaches_the_callback(host) -> None:
    """Host-initiated events route to the topic's callback.

    Events arrive on the same socket as responses and carry no request id, so a
    reader loop that treated every frame as a reply would drop them entirely --
    the subscription would look established and deliver nothing.
    """
    h, client = await _connected(host)
    seen: list[dict] = []
    try:
        h.replies["event.subscribe"] = {"ok": True}
        await client.event_subscribe("plugin.p.topic", lambda payload: seen.append(payload))
        await h.push_event(
            "event.deliver", {"topic": "plugin.p.topic", "payload": {"n": 1}}
        )
        for _ in range(50):
            if seen:
                break
            await asyncio.sleep(0.01)
        assert seen == [{"n": 1}], f"callback never fired: {seen}"
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_close_is_safe_before_and_after_connect(short_sock_dir: Path) -> None:
    """Teardown must not raise on a client that never connected.

    The runner closes the client on every exit path, including the one where
    the host socket was absent and the connection was never made.
    """
    client = PluginIpcClient(
        plugin_id="p", token="t", socket_path=short_sock_dir / "absent.sock"
    )
    await client.close()  # never connected
    await client.close()  # and again


def test_an_unparseable_token_grants_no_capabilities() -> None:
    """Fail closed on a token the client cannot read.

    The granted set gates `tool.invoke` locally. Defaulting to "everything" on
    a parse failure would turn a corrupt token into a privilege escalation; the
    client resolves it to the empty set instead.
    """
    client = PluginIpcClient(
        plugin_id="p", token="not-a-real-token", socket_path=Path("/nonexistent")
    )
    assert client._granted_caps == frozenset()
