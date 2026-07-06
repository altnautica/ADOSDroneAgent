"""Tests for the mDNS ``DiscoveryService``.

The real ``zeroconf.asyncio`` symbols are mocked at the module level
inside the ``register`` / ``update_txt`` paths so the test does not
bind a real socket or hit the network. The local-IP probe is also
short-circuited so behavior is the same on macOS dev hosts and Linux
CI runners.

Companion to ``tests/test_discovery.py`` (which covers the
``unregister`` await-broadcast contract). This file covers TXT-record
shape, registration / update lifecycle, service-type wiring, and the
graceful-degradation path when zeroconf raises.
"""

from __future__ import annotations

import socket
import sys
from types import SimpleNamespace
from typing import Any
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.services.discovery import SERVICE_TYPE, DiscoveryService

# ---------------------------------------------------------------------------
# Constants + helpers
# ---------------------------------------------------------------------------


_DEVICE_ID = "deadbeefcafe1234567890"
_EXPECTED_SHORT = _DEVICE_ID[:6].lower()


def _patched_zeroconf(monkeypatch: pytest.MonkeyPatch) -> tuple[MagicMock, MagicMock]:
    """Stub ``zeroconf`` and ``zeroconf.asyncio`` so register/update don't hit the wire."""
    info_class = MagicMock(name="AsyncServiceInfo")
    az_instance = MagicMock(name="AsyncZeroconf-inst")
    az_instance.async_register_service = AsyncMock()
    az_instance.async_update_service = AsyncMock()

    # async_unregister_service must return an awaitable. Use a per-call
    # async factory so each invocation gets its own fresh coroutine
    # (avoids "coroutine was never awaited" leaks across tests).
    async def _broadcast() -> None:
        return None

    async def _unregister(_info: Any) -> Any:
        return _broadcast()

    az_instance.async_unregister_service = _unregister
    az_instance.async_close = AsyncMock()
    az_class = MagicMock(name="AsyncZeroconf", return_value=az_instance)

    fake_async = SimpleNamespace(
        AsyncServiceInfo=info_class,
        AsyncZeroconf=az_class,
    )
    fake_zc = SimpleNamespace(
        IPVersion=SimpleNamespace(V4Only="V4Only"),
        asyncio=fake_async,
    )

    monkeypatch.setitem(sys.modules, "zeroconf", fake_zc)
    monkeypatch.setitem(sys.modules, "zeroconf.asyncio", fake_async)
    return info_class, az_class


# ---------------------------------------------------------------------------
# Constructor + computed properties
# ---------------------------------------------------------------------------


def test_service_type_constant() -> None:
    """The package-level constant is the canonical mDNS service type."""
    assert SERVICE_TYPE == "_ados._tcp.local."


def test_mdns_hostname_uses_the_real_system_hostname() -> None:
    # The reported reach name must be the resolvable system hostname that
    # avahi actually publishes, never a constructed `ados-<id>.local` that
    # nothing publishes as an A-record.
    svc = DiscoveryService(device_id=_DEVICE_ID)
    with patch("ados.services.discovery.socket.gethostname", return_value="drone-rig"):
        assert svc.mdns_hostname == "drone-rig.local"


def test_mdns_hostname_falls_back_to_device_id_when_hostname_unusable() -> None:
    # Only an unusable hostname (empty / localhost / loopback literal) falls
    # back to the device-id form.
    svc = DiscoveryService(device_id=_DEVICE_ID)
    for bad in ("", "localhost", "127.0.0.1"):
        with patch("ados.services.discovery.socket.gethostname", return_value=bad):
            assert svc.mdns_hostname == f"ados-{_EXPECTED_SHORT}.local"


def test_default_port_and_name() -> None:
    """Defaults align with the agent's REST API port and a placeholder name."""
    svc = DiscoveryService(device_id=_DEVICE_ID)
    assert svc._port == 8080
    assert svc._name == "my-drone"
    assert svc._version == "0.2.0"
    assert svc._board == "unknown"


def test_local_ip_falls_back_to_loopback_on_oserror() -> None:
    """A network-down host (CI sandbox) must still get a usable string."""
    svc = DiscoveryService(device_id=_DEVICE_ID)
    with patch("socket.socket") as sock_factory:
        sock_factory.return_value.connect.side_effect = OSError("no route")
        assert svc._get_local_ip() == "127.0.0.1"


# ---------------------------------------------------------------------------
# TXT record builder
# ---------------------------------------------------------------------------


def test_txt_records_unpaired_includes_pair_code() -> None:
    """The pair code surfaces only while ``paired`` is False."""
    svc = DiscoveryService(
        device_id=_DEVICE_ID, port=9090, name="bench", version="1.2.3", board="rpi4b"
    )
    txt = svc._build_txt_records(paired=False, code="123456")
    assert txt["paired"] == "false"
    assert txt["code"] == "123456"
    assert "owner" not in txt
    assert txt["device_id"] == _DEVICE_ID
    assert txt["version"] == "1.2.3"
    assert txt["board"] == "rpi4b"
    assert txt["name"] == "bench"


def test_txt_records_paired_drops_code_adds_owner() -> None:
    svc = DiscoveryService(device_id=_DEVICE_ID)
    txt = svc._build_txt_records(paired=True, code="ignored", owner="owner-123")
    assert txt["paired"] == "true"
    assert "code" not in txt
    assert txt["owner"] == "owner-123"


def test_txt_records_optional_profile_and_role() -> None:
    svc = DiscoveryService(device_id=_DEVICE_ID)
    txt = svc._build_txt_records(
        paired=True, owner="o", profile="ground_station", role="relay"
    )
    assert txt["profile"] == "ground_station"
    assert txt["role"] == "relay"


# ---------------------------------------------------------------------------
# Registration lifecycle
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_register_uses_configured_port_and_service_type(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """ServiceInfo is constructed with the right type, name, port, and addresses."""
    info_class, az_class = _patched_zeroconf(monkeypatch)
    svc = DiscoveryService(
        device_id=_DEVICE_ID, port=9090, name="bench", version="1.2.3", board="rpi4b"
    )

    with patch.object(svc, "_get_local_ip", return_value="192.168.1.10"):
        await svc.register(paired=False, code="654321", profile="drone")

    assert info_class.call_count == 1
    args, kwargs = info_class.call_args
    assert args[0] == SERVICE_TYPE
    assert args[1] == f"ADOS-{_EXPECTED_SHORT}.{SERVICE_TYPE}"
    assert kwargs["port"] == 9090
    assert kwargs["addresses"] == [socket.inet_aton("192.168.1.10")]
    assert kwargs["server"] == f"ados-{_EXPECTED_SHORT}.local."

    # TXT records carry the pair code and profile.
    properties = kwargs["properties"]
    assert properties["paired"] == "false"
    assert properties["code"] == "654321"
    assert properties["profile"] == "drone"
    assert properties["board"] == "rpi4b"

    # The AsyncZeroconf instance was asked to register the service.
    az_instance = az_class.return_value
    az_instance.async_register_service.assert_awaited_once()


@pytest.mark.asyncio
async def test_register_zeroconf_failure_does_not_raise(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """mDNS is optional. A failed registration must not crash the agent."""
    info_class, az_class = _patched_zeroconf(monkeypatch)
    az_class.return_value.async_register_service.side_effect = RuntimeError(
        "zeroconf bind failed"
    )

    svc = DiscoveryService(device_id=_DEVICE_ID)
    with patch.object(svc, "_get_local_ip", return_value="10.0.0.5"):
        # Must not raise.
        await svc.register(paired=False, code="111111")

    # On failure the internal handles are cleared so a follow-up
    # update_txt / unregister is a no-op.
    assert svc._zeroconf is None
    assert svc._info is None


@pytest.mark.asyncio
async def test_register_missing_zeroconf_module_is_swallowed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """If the package import itself blows up, the agent stays alive."""
    # Force the conditional ``from zeroconf import ...`` to raise.
    real_import = __builtins__["__import__"] if isinstance(__builtins__, dict) else __builtins__.__import__

    def _blocking_import(name: str, *args: Any, **kwargs: Any) -> Any:
        if name == "zeroconf" or name.startswith("zeroconf."):
            raise ImportError("simulated missing dependency")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr("builtins.__import__", _blocking_import)

    svc = DiscoveryService(device_id=_DEVICE_ID)
    await svc.register()  # must not raise
    assert svc._zeroconf is None


# ---------------------------------------------------------------------------
# TXT update on pairing state change
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_update_txt_noop_when_not_registered() -> None:
    """Without a prior register, update_txt is a silent no-op."""
    svc = DiscoveryService(device_id=_DEVICE_ID)
    # No exception, nothing patched: the early return keeps it cheap.
    await svc.update_txt(paired=True, owner="x")


@pytest.mark.asyncio
async def test_update_txt_swaps_in_paired_records(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """After pairing flips, the new ServiceInfo carries ``owner`` not ``code``."""
    info_class, az_class = _patched_zeroconf(monkeypatch)
    svc = DiscoveryService(device_id=_DEVICE_ID)

    with patch.object(svc, "_get_local_ip", return_value="10.0.0.1"):
        await svc.register(paired=False, code="123456")

        original_info = svc._info
        info_class.reset_mock()

        # The mocked info objects need a ``.name`` so update_txt can reuse it.
        original_info.name = f"ADOS-{_EXPECTED_SHORT}.{SERVICE_TYPE}"

        await svc.update_txt(paired=True, owner="owner-1", role="direct")

    assert info_class.call_count == 1
    _args, kwargs = info_class.call_args
    properties = kwargs["properties"]
    assert properties["paired"] == "true"
    assert properties["owner"] == "owner-1"
    assert properties["role"] == "direct"
    assert "code" not in properties

    az_instance = az_class.return_value
    az_instance.async_update_service.assert_awaited_once()


# ---------------------------------------------------------------------------
# mdns_enabled-style opt-out
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_caller_can_skip_registration_without_side_effects(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The DiscoveryService is dormant until register() is called.

    The agent's ``DiscoveryConfig.mdns_enabled=False`` path is enforced
    by the caller (the discovery service main loop) by simply not
    invoking register(). This test pins that contract: a fresh service
    with no register call holds no zeroconf handle.
    """
    info_class, _ = _patched_zeroconf(monkeypatch)
    svc = DiscoveryService(device_id=_DEVICE_ID)
    # No register() call.
    assert svc._zeroconf is None
    assert svc._info is None
    assert info_class.call_count == 0


# ---------------------------------------------------------------------------
# Unregister cleanup contract (sibling to tests/test_discovery.py)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_unregister_is_idempotent_when_never_registered() -> None:
    svc = DiscoveryService(device_id=_DEVICE_ID)
    # Should not raise.
    await svc.unregister()
    await svc.unregister()
    assert svc._zeroconf is None
    assert svc._info is None
