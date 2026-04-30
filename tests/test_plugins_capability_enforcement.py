"""Capability runtime enforcement tests.

Two layers covered:

1. ``ados.plugins.capabilities`` helpers against supervisor state
   (``get_granted_caps``, ``has_capability``, ``require_capability``).
2. The IPC server's per-method capability gate. The dispatcher rejects
   ungranted callers with ``capability_denied: <cap>`` before the
   handler is reached. Granted callers reach the stub handler and get
   the not_implemented response.
"""

from __future__ import annotations

import asyncio
import shutil
import tempfile
from pathlib import Path
from typing import Any

import pytest

from ados.plugins.capabilities import (
    AGENT_CAPABILITIES,
    ENFORCED_AGENT_CAPABILITIES,
    get_granted_caps,
    has_capability,
    is_known_agent_capability,
    require_capability,
)
from ados.plugins.errors import CapabilityDenied
from ados.plugins.events import EventBus
from ados.plugins.ipc_client import PluginIpcClient
from ados.plugins.ipc_server import PluginIpcServer
from ados.plugins.rpc import Envelope, TokenIssuer, encode_frame, read_frame
from ados.plugins.state import PermissionGrant, PluginInstall


PLUGIN_ID = "com.example.gated"


# ---------------------------------------------------------------------
# Helpers and fixtures
# ---------------------------------------------------------------------


class _StubSupervisor:
    """Minimal supervisor stand-in for the helpers.

    The real supervisor exposes ``find_install``; the helpers only call
    that one method, so a lightweight stub keeps the tests fast and
    free of /var/ados side effects.
    """

    def __init__(self, installs: list[PluginInstall]) -> None:
        self._installs = installs

    def find_install(self, plugin_id: str) -> PluginInstall | None:
        for inst in self._installs:
            if inst.plugin_id == plugin_id:
                return inst
        return None


def _install_with_perms(plugin_id: str, **grants: bool) -> PluginInstall:
    return PluginInstall(
        plugin_id=plugin_id,
        version="1.0.0",
        source="local_file",
        source_uri=None,
        signer_id=None,
        manifest_hash="x" * 64,
        status="installed",
        installed_at=0,
        permissions={
            pid: PermissionGrant(granted=g, granted_at=0 if g else None)
            for pid, g in grants.items()
        },
    )


# ---------------------------------------------------------------------
# Catalog
# ---------------------------------------------------------------------


def test_catalog_size_matches_spec() -> None:
    assert len(AGENT_CAPABILITIES) == 29


def test_only_event_caps_are_enforced_today() -> None:
    assert ENFORCED_AGENT_CAPABILITIES == frozenset(
        {"event.publish", "event.subscribe"}
    )


def test_is_known_agent_capability() -> None:
    assert is_known_agent_capability("event.publish")
    assert is_known_agent_capability("mavlink.read")
    assert not is_known_agent_capability("not.a.real.cap")


# ---------------------------------------------------------------------
# State-driven helpers
# ---------------------------------------------------------------------


def test_get_granted_caps_returns_only_granted() -> None:
    sup = _StubSupervisor(
        [
            _install_with_perms(
                PLUGIN_ID,
                **{"telemetry.read": True, "mission.write": False},
            )
        ]
    )
    assert get_granted_caps(sup, PLUGIN_ID) == {"telemetry.read"}


def test_get_granted_caps_unknown_plugin_returns_empty() -> None:
    sup = _StubSupervisor([])
    assert get_granted_caps(sup, "no.such.plugin") == set()


def test_has_capability_true_when_granted() -> None:
    sup = _StubSupervisor(
        [_install_with_perms(PLUGIN_ID, **{"mission.read": True})]
    )
    assert has_capability(sup, PLUGIN_ID, "mission.read")
    assert not has_capability(sup, PLUGIN_ID, "mission.write")


def test_require_capability_raises_on_missing() -> None:
    sup = _StubSupervisor([_install_with_perms(PLUGIN_ID)])
    with pytest.raises(CapabilityDenied) as excinfo:
        require_capability(sup, PLUGIN_ID, "vehicle.command")
    assert excinfo.value.plugin_id == PLUGIN_ID
    assert excinfo.value.capability == "vehicle.command"


def test_require_capability_silent_when_granted() -> None:
    sup = _StubSupervisor(
        [_install_with_perms(PLUGIN_ID, **{"recording.write": True})]
    )
    require_capability(sup, PLUGIN_ID, "recording.write")  # no raise


# ---------------------------------------------------------------------
# IPC dispatch gate
# ---------------------------------------------------------------------


@pytest.fixture
def short_sock_dir():
    base = Path(tempfile.mkdtemp(prefix="adpcap", dir="/tmp"))
    try:
        yield base
    finally:
        shutil.rmtree(base, ignore_errors=True)


async def _connected_client(
    short_sock_dir: Path, granted: set[str]
) -> tuple[PluginIpcServer, PluginIpcClient]:
    bus = EventBus()
    issuer = TokenIssuer()
    server = PluginIpcServer(
        bus=bus, token_issuer=issuer, socket_dir=short_sock_dir
    )
    sock = await server.start_for_plugin(PLUGIN_ID)
    token = issuer.mint(plugin_id=PLUGIN_ID, granted_caps=granted)
    client = PluginIpcClient(
        plugin_id=PLUGIN_ID, token=token.to_string(), socket_path=sock
    )
    await client.connect()
    return server, client


async def _call_method(
    client: PluginIpcClient, method: str, args: dict[str, Any]
) -> Envelope:
    """Send a request via the client's correlation-aware path.

    Uses the private ``_send_request`` so the response is matched by
    request_id through the client's reader loop, avoiding double-read
    contention with the high-level ``ping`` / ``event_*`` methods.
    """
    return await client._send_request(method, capability=method, args=args)


@pytest.mark.asyncio
async def test_gated_method_denied_without_capability(
    short_sock_dir: Path,
) -> None:
    server, client = await _connected_client(short_sock_dir, granted=set())
    try:
        with pytest.raises(CapabilityDenied) as excinfo:
            await _call_method(client, "telemetry.subscribe", {})
        assert excinfo.value.capability == "telemetry.read"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_gated_method_reaches_handler_when_granted(
    short_sock_dir: Path,
) -> None:
    server, client = await _connected_client(
        short_sock_dir, granted={"telemetry.read"}
    )
    try:
        resp = await _call_method(client, "telemetry.subscribe", {})
        # Stub returns not_implemented in args; no error envelope.
        assert resp.error is None
        assert resp.args.get("error") == "not_implemented"
        assert resp.args.get("method") == "telemetry.subscribe"
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_each_gated_method_rejects_with_correct_cap(
    short_sock_dir: Path,
) -> None:
    cases = [
        ("telemetry.subscribe", "telemetry.read"),
        ("telemetry.extend", "telemetry.extend"),
        ("mission.read", "mission.read"),
        ("mission.write", "mission.write"),
        ("recording.start", "recording.write"),
        ("recording.stop", "recording.write"),
        ("mavlink.subscribe", "mavlink.read"),
        ("mavlink.send", "mavlink.write"),
    ]
    server, client = await _connected_client(short_sock_dir, granted=set())
    try:
        for method, expected_cap in cases:
            with pytest.raises(CapabilityDenied) as excinfo:
                await _call_method(client, method, {})
            assert excinfo.value.capability == expected_cap, (
                f"{method} should cite {expected_cap}, "
                f"got {excinfo.value.capability}"
            )
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)


@pytest.mark.asyncio
async def test_ungated_method_runs_without_check(
    short_sock_dir: Path,
) -> None:
    """ping has no requires; should work even with empty caps."""
    server, client = await _connected_client(short_sock_dir, granted=set())
    try:
        resp = await client.ping()
        assert resp["pong"] is True
    finally:
        await client.close()
        await server.stop_for_plugin(PLUGIN_ID)
