"""End-to-end IPC bridge tests.

Boots a real :class:`PluginIpcServer` on a tmp Unix socket, connects a
real :class:`PluginIpcClient` over it, drives the published RPC
methods, and asserts capability-token enforcement at the handshake.

These tests exercise the supervisor's serving surface and the
runner's client surface against each other in a single process.

Sockets are placed under ``/tmp/<short>`` because macOS caps
``AF_UNIX`` path length around 104 bytes and pytest's ``tmp_path``
already eats most of that budget.
"""

from __future__ import annotations

import asyncio
import shutil
import tempfile
from pathlib import Path

import pytest

from ados.plugins.events import Event, EventBus, now_ms
from ados.plugins.ipc_client import PluginContext, PluginIpcClient
from ados.plugins.ipc_server import PluginIpcServer
from ados.plugins.rpc import (
    Envelope,
    TokenIssuer,
    encode_frame,
    read_frame,
)

PLUGIN_ID = "com.example.basic"


@pytest.fixture
def short_sock_dir():
    """Short /tmp-rooted directory to keep AF_UNIX paths under 104 bytes."""
    base = Path(tempfile.mkdtemp(prefix="adp", dir="/tmp"))
    try:
        yield base
    finally:
        shutil.rmtree(base, ignore_errors=True)


# ---------------------------------------------------------------------
# Test harness
# ---------------------------------------------------------------------


@pytest.fixture
async def harness(short_sock_dir: Path):
    bus = EventBus()
    issuer = TokenIssuer()
    server = PluginIpcServer(
        bus=bus, token_issuer=issuer, socket_dir=short_sock_dir
    )
    sock_path = await server.start_for_plugin(PLUGIN_ID)
    token = issuer.mint(
        plugin_id=PLUGIN_ID,
        granted_caps={"event.publish", "event.subscribe"},
    )
    client = PluginIpcClient(
        plugin_id=PLUGIN_ID,
        token=token.to_string(),
        socket_path=sock_path,
    )
    await client.connect()
    yield bus, issuer, server, client, token, sock_path
    await client.close()
    await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Happy paths
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_ping_round_trip(harness) -> None:
    _, _, _, client, _, _ = harness
    pong = await client.ping()
    assert pong["pong"] is True
    assert pong["plugin_id"] == PLUGIN_ID


@pytest.mark.asyncio
async def test_event_publish_routes_to_bus(harness) -> None:
    bus, _, _, client, _, _ = harness

    received: list[Event] = []

    async def reader() -> None:
        async for evt in bus.subscribe(f"plugin.{PLUGIN_ID}.alert"):
            received.append(evt)
            return

    task = asyncio.create_task(reader())
    await asyncio.sleep(0)
    delivered = await client.event_publish(
        f"plugin.{PLUGIN_ID}.alert", {"level": 3}
    )
    await asyncio.wait_for(task, timeout=1.0)
    assert delivered == 1
    assert received[0].payload == {"level": 3}
    assert received[0].publisher_plugin_id == PLUGIN_ID


@pytest.mark.asyncio
async def test_event_subscribe_delivers_to_callback(harness) -> None:
    bus, _, _, client, _, _ = harness

    got: list[dict] = []
    delivered = asyncio.Event()

    async def cb(payload: dict) -> None:
        got.append(payload)
        delivered.set()

    await client.event_subscribe("vehicle.armed", cb)
    # Allow the supervisor's pump_subscription task to wire up.
    await asyncio.sleep(0.05)
    await bus.publish(
        Event(
            topic="vehicle.armed",
            timestamp_ms=now_ms(),
            publisher_plugin_id=None,
            payload={"by": "operator"},
        )
    )
    await asyncio.wait_for(delivered.wait(), timeout=1.0)
    assert got == [{"by": "operator"}]


# ---------------------------------------------------------------------
# Capability enforcement
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_publish_to_reserved_namespace_returns_error(harness) -> None:
    _, _, _, client, _, _ = harness
    from ados.plugins.errors import CapabilityDenied

    with pytest.raises(CapabilityDenied):
        # Reserved host-only namespace; the server's capability gate rejects.
        await client.event_publish("vehicle.armed", {"by": "intruder"})


@pytest.mark.asyncio
async def test_subscribe_to_other_namespace_returns_error(harness) -> None:
    _, _, _, client, _, _ = harness
    from ados.plugins.errors import CapabilityDenied

    async def _cb(_: dict) -> None:
        return

    with pytest.raises(CapabilityDenied):
        await client.event_subscribe("plugin.com.other.y.alert", _cb)


# ---------------------------------------------------------------------
# Handshake / token validation
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_handshake_with_wrong_plugin_id_rejected(short_sock_dir: Path) -> None:
    bus = EventBus()
    issuer = TokenIssuer()
    server = PluginIpcServer(
        bus=bus, token_issuer=issuer, socket_dir=short_sock_dir
    )
    sock_path = await server.start_for_plugin("com.example.real")
    # Mint a token bound to a DIFFERENT plugin id.
    bad_token = issuer.mint(
        plugin_id="com.example.impostor",
        granted_caps={"event.publish"},
    )
    reader, writer = await asyncio.open_unix_connection(str(sock_path))
    hello = Envelope(
        type="request",
        method="hello",
        capability="",
        args={},
        request_id="r1",
        token=bad_token.to_string(),
    )
    writer.write(encode_frame(hello))
    await writer.drain()
    response = await read_frame(reader)
    assert response is not None
    assert response.error is not None
    assert "does not match" in response.error
    writer.close()
    await writer.wait_closed()
    await server.stop_for_plugin("com.example.real")


@pytest.mark.asyncio
async def test_handshake_with_expired_token_rejected(short_sock_dir: Path) -> None:
    bus = EventBus()
    issuer = TokenIssuer()
    server = PluginIpcServer(
        bus=bus, token_issuer=issuer, socket_dir=short_sock_dir
    )
    sock_path = await server.start_for_plugin(PLUGIN_ID)
    token = issuer.mint(
        plugin_id=PLUGIN_ID, granted_caps={"event.publish"}, ttl_seconds=1
    )
    # Force-age the token by reaching past expires_at.
    import time as _t

    _t.sleep(1.1)
    reader, writer = await asyncio.open_unix_connection(str(sock_path))
    hello = Envelope(
        type="request",
        method="hello",
        capability="",
        args={},
        request_id="r1",
        token=token.to_string(),
    )
    writer.write(encode_frame(hello))
    await writer.drain()
    response = await read_frame(reader)
    assert response is not None
    assert response.error is not None
    assert "invalid" in response.error or "expired" in response.error
    writer.close()
    await writer.wait_closed()
    await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# Per-request token expiry enforcement
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_request_after_token_expiry_returns_token_expired(
    short_sock_dir: Path,
) -> None:
    """An aged-out token must be rejected on the next request even
    though the handshake accepted a then-valid token."""
    bus = EventBus()
    issuer = TokenIssuer()
    server = PluginIpcServer(
        bus=bus, token_issuer=issuer, socket_dir=short_sock_dir
    )
    sock_path = await server.start_for_plugin(PLUGIN_ID)

    # Mint a token with a short TTL so we can age past it inside the
    # test without hanging the suite.
    token = issuer.mint(
        plugin_id=PLUGIN_ID,
        granted_caps={"event.publish"},
        ttl_seconds=1,
    )
    reader, writer = await asyncio.open_unix_connection(str(sock_path))
    hello = Envelope(
        type="request",
        method="hello",
        capability="",
        args={},
        request_id="r1",
        token=token.to_string(),
    )
    writer.write(encode_frame(hello))
    await writer.drain()
    handshake_resp = await read_frame(reader)
    assert handshake_resp is not None
    assert handshake_resp.error is None

    # Wait until the token has aged past expires_at.
    await asyncio.sleep(1.2)

    ping = Envelope(
        type="request",
        method="ping",
        capability="",
        args={},
        request_id="r2",
        token=token.to_string(),
    )
    writer.write(encode_frame(ping))
    await writer.drain()
    response = await read_frame(reader)
    assert response is not None
    assert response.error == "token_expired"

    writer.close()
    await writer.wait_closed()
    await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# PluginContext public surface
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plugin_context_events_publish(harness) -> None:
    bus, _, _, client, _, _ = harness
    ctx = PluginContext(
        plugin_id=PLUGIN_ID,
        plugin_version="0.1.0",
        config={},
        ipc=client,
    )

    received: list[Event] = []

    async def reader() -> None:
        async for evt in bus.subscribe(f"plugin.{PLUGIN_ID}.health"):
            received.append(evt)
            return

    task = asyncio.create_task(reader())
    await asyncio.sleep(0)
    delivered = await ctx.events.publish(
        f"plugin.{PLUGIN_ID}.health", {"status": "ok"}
    )
    await asyncio.wait_for(task, timeout=1.0)
    assert delivered == 1
    assert received[0].payload == {"status": "ok"}


@pytest.mark.asyncio
async def test_plugin_context_exposes_compute_facade(harness) -> None:
    """``ctx.compute`` is the compute-offload facade (the M8 attachment),
    alongside the vision facade, so a plugin can submit jobs to a paired
    compute node through the host's gated dispatch."""
    from ados.sdk.compute import ComputeClient
    from ados.sdk.vision import VisionClient

    _, _, _, client, _, _ = harness
    ctx = PluginContext(
        plugin_id=PLUGIN_ID,
        plugin_version="0.1.0",
        config={},
        ipc=client,
    )
    assert isinstance(ctx.compute, ComputeClient)
    assert isinstance(ctx.vision, VisionClient)


# ---------------------------------------------------------------------
# Mutual exclusion with the native plugin host
# ---------------------------------------------------------------------


@pytest.mark.asyncio
async def test_yields_to_native_host_does_not_bind(short_sock_dir: Path) -> None:
    """When the native host is the active socket owner, the packaged server
    yields: start_for_plugin returns the path but binds nothing, so no socket
    is created and a connect attempt fails."""
    server = PluginIpcServer(
        bus=EventBus(),
        token_issuer=TokenIssuer(),
        socket_dir=short_sock_dir,
        native_owner_check=lambda: True,
    )
    sock_path = await server.start_for_plugin(PLUGIN_ID)
    # The path is returned but nothing is bound.
    assert not sock_path.exists()
    with pytest.raises((ConnectionRefusedError, FileNotFoundError, OSError)):
        _, writer = await asyncio.open_unix_connection(str(sock_path))
        writer.close()
        await writer.wait_closed()


@pytest.mark.asyncio
async def test_yield_then_stop_does_not_unlink_foreign_socket(
    short_sock_dir: Path,
) -> None:
    """A yielded server must not remove the socket file the native host owns.

    Simulate the native host's live socket as a plain file at the path, yield,
    then stop. The foreign file must survive (we never bound it)."""
    foreign = short_sock_dir / f"{PLUGIN_ID}.sock"
    foreign.write_text("native-host-socket-placeholder")
    server = PluginIpcServer(
        bus=EventBus(),
        token_issuer=TokenIssuer(),
        socket_dir=short_sock_dir,
        native_owner_check=lambda: True,
    )
    await server.start_for_plugin(PLUGIN_ID)
    assert foreign.exists()
    await server.stop_for_plugin(PLUGIN_ID)
    assert foreign.exists()


@pytest.mark.asyncio
async def test_binds_when_native_host_pinned_off(short_sock_dir: Path) -> None:
    """With the native host pinned off (the check returns False), the packaged
    server binds and serves the socket as before."""
    issuer = TokenIssuer()
    server = PluginIpcServer(
        bus=EventBus(),
        token_issuer=issuer,
        socket_dir=short_sock_dir,
        native_owner_check=lambda: False,
    )
    sock_path = await server.start_for_plugin(PLUGIN_ID)
    try:
        assert sock_path.exists()
        token = issuer.mint(plugin_id=PLUGIN_ID, granted_caps={"event.publish"})
        client = PluginIpcClient(
            plugin_id=PLUGIN_ID,
            token=token.to_string(),
            socket_path=sock_path,
        )
        await client.connect()
        await client.close()
    finally:
        await server.stop_for_plugin(PLUGIN_ID)


# ---------------------------------------------------------------------
# MCP tool invocation (host -> plugin request/response)
# ---------------------------------------------------------------------


async def _tool_harness(sock_dir: Path, caps: set[str]):
    """Boot a server + a client whose token carries ``caps``. Returns
    (server, client, teardown)."""
    bus = EventBus()
    issuer = TokenIssuer()
    server = PluginIpcServer(bus=bus, token_issuer=issuer, socket_dir=sock_dir)
    sock_path = await server.start_for_plugin(PLUGIN_ID)
    token = issuer.mint(plugin_id=PLUGIN_ID, granted_caps=caps)
    client = PluginIpcClient(
        plugin_id=PLUGIN_ID, token=token.to_string(), socket_path=sock_path
    )
    await client.connect()

    async def teardown() -> None:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)

    return server, client, teardown


@pytest.mark.asyncio
async def test_tool_invoke_round_trips(short_sock_dir: Path) -> None:
    server, client, teardown = await _tool_harness(short_sock_dir, {"mcp.expose"})
    try:
        client.register_tool("echo", lambda args: {"echoed": args})

        async def _adder(args: dict) -> dict:
            return {"sum": args["a"] + args["b"]}

        client.register_tool("add", _adder)

        assert await server.invoke_tool(PLUGIN_ID, "echo", {"x": 1}) == {
            "echoed": {"x": 1}
        }
        assert await server.invoke_tool(PLUGIN_ID, "add", {"a": 2, "b": 3}) == {
            "sum": 5
        }
    finally:
        await teardown()


@pytest.mark.asyncio
async def test_tool_invoke_non_dict_result_is_wrapped(short_sock_dir: Path) -> None:
    server, client, teardown = await _tool_harness(short_sock_dir, {"mcp.expose"})
    try:
        client.register_tool("answer", lambda _args: 42)
        assert await server.invoke_tool(PLUGIN_ID, "answer", {}) == {"result": 42}
    finally:
        await teardown()


@pytest.mark.asyncio
async def test_tool_invoke_unknown_tool_errors(short_sock_dir: Path) -> None:
    from ados.plugins.ipc_server import _RpcError

    server, _client, teardown = await _tool_harness(short_sock_dir, {"mcp.expose"})
    try:
        with pytest.raises(_RpcError, match="tool_not_found"):
            await server.invoke_tool(PLUGIN_ID, "nope", {})
    finally:
        await teardown()


@pytest.mark.asyncio
async def test_tool_invoke_handler_exception_surfaces(short_sock_dir: Path) -> None:
    from ados.plugins.ipc_server import _RpcError

    server, client, teardown = await _tool_harness(short_sock_dir, {"mcp.expose"})
    try:
        def _boom(_args: dict) -> dict:
            raise ValueError("bad")

        client.register_tool("boom", _boom)
        with pytest.raises(_RpcError, match="tool_error"):
            await server.invoke_tool(PLUGIN_ID, "boom", {})
    finally:
        await teardown()


@pytest.mark.asyncio
async def test_tool_invoke_without_mcp_expose_is_denied(short_sock_dir: Path) -> None:
    from ados.plugins.errors import CapabilityDenied

    # The token lacks mcp.expose: the host-side gate refuses before sending.
    server, client, teardown = await _tool_harness(short_sock_dir, {"event.publish"})
    try:
        client.register_tool("echo", lambda args: {"echoed": args})
        with pytest.raises(CapabilityDenied):
            await server.invoke_tool(PLUGIN_ID, "echo", {})
    finally:
        await teardown()


@pytest.mark.asyncio
async def test_tool_invoke_plugin_not_running_errors(short_sock_dir: Path) -> None:
    from ados.plugins.ipc_server import _RpcError

    bus = EventBus()
    issuer = TokenIssuer()
    server = PluginIpcServer(bus=bus, token_issuer=issuer, socket_dir=short_sock_dir)
    with pytest.raises(_RpcError, match="plugin_not_running"):
        await server.invoke_tool(PLUGIN_ID, "echo", {})


@pytest.mark.asyncio
async def test_ctx_tools_decorator_registers_and_invokes(short_sock_dir: Path) -> None:
    """The SDK ctx.tools facade registers a handler that a host invoke reaches."""
    server, client, teardown = await _tool_harness(short_sock_dir, {"mcp.expose"})
    try:
        ctx = PluginContext(
            plugin_id=PLUGIN_ID, plugin_version="0.1.0", config={}, ipc=client
        )

        @ctx.tools.tool("greet")
        def _greet(args: dict) -> dict:
            return {"hello": args.get("name", "world")}

        assert await server.invoke_tool(PLUGIN_ID, "greet", {"name": "ada"}) == {
            "hello": "ada"
        }
    finally:
        await teardown()
