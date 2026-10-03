"""Tests for the mDNS ``DiscoveryService``.

The real ``zeroconf.asyncio`` symbols are mocked at the module level
inside the ``register`` / ``refresh`` paths so the test does not
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


@pytest.fixture(autouse=True)
def _no_avahi(monkeypatch: pytest.MonkeyPatch) -> None:
    """No avahi on the test host: the naming falls back to the system hostname."""
    monkeypatch.setattr("ados.services.discovery._avahi_host_fqdn", lambda: None)


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


def test_a_dotted_hostname_is_named_by_its_first_label() -> None:
    # An mDNS responder publishes `<first label>.local`; a unicast DNS name
    # such as `drone-rig.lan` is not something multicast DNS resolves.
    svc = DiscoveryService(device_id=_DEVICE_ID)
    for dotted in ("drone-rig.lan", "drone-rig.local", "drone-rig.lan."):
        with patch("ados.services.discovery.socket.gethostname", return_value=dotted):
            assert svc.mdns_hostname == "drone-rig.local"


def test_avahis_published_name_wins_over_the_hostname(monkeypatch: pytest.MonkeyPatch) -> None:
    # After a collision avahi renames this host to `<host>-2.local`; the plain
    # hostname then answers for the other board.
    monkeypatch.setattr(
        "ados.services.discovery._avahi_host_fqdn", lambda: "drone-rig-2.local"
    )
    svc = DiscoveryService(device_id=_DEVICE_ID)
    with patch("ados.services.discovery.socket.gethostname", return_value="drone-rig"):
        assert svc.mdns_hostname == "drone-rig-2.local"


def test_avahis_name_is_read_from_the_busctl_reply() -> None:
    from ados.services.discovery import _parse_busctl_string

    assert _parse_busctl_string('s "drone-rig-2.local"\n') == "drone-rig-2.local"
    for bad in ("", 's ""', 's "localhost"', 's "drone-rig.lan"', "u 5"):
        assert _parse_busctl_string(bad) is None


def test_mdns_hostname_falls_back_to_device_id_when_hostname_unusable() -> None:
    # Only an unusable hostname (empty / localhost / loopback literal) falls
    # back to the device-id form.
    svc = DiscoveryService(device_id=_DEVICE_ID)
    for bad in ("", "localhost", "localhost.localdomain", "127.0.0.1"):
        with patch("ados.services.discovery.socket.gethostname", return_value=bad):
            assert svc.mdns_hostname == f"ados-{_EXPECTED_SHORT}.local"


def test_default_port_and_name() -> None:
    """Defaults align with the agent's REST API port and a placeholder name."""
    svc = DiscoveryService(device_id=_DEVICE_ID)
    assert svc._port == 8080
    assert svc._name == "my-drone"
    assert svc._version == "0.2.0"
    assert svc._board == "unknown"


def test_local_addresses_come_from_the_interfaces_and_never_loopback() -> None:
    """An AP-only node has no default route; its AP address is still advertised,
    and loopback never is."""
    adapters = [
        SimpleNamespace(ips=[SimpleNamespace(ip="127.0.0.1"), SimpleNamespace(ip=("::1", 0, 0))]),
        SimpleNamespace(ips=[SimpleNamespace(ip="192.168.4.1")]),
    ]
    svc = DiscoveryService(device_id=_DEVICE_ID)
    with patch("ifaddr.get_adapters", return_value=adapters):
        assert svc._local_addresses() == ["192.168.4.1"]


@pytest.mark.asyncio
async def test_a_node_without_an_address_registers_once_one_appears(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Registration is retried by refresh: a node whose network came up after
    the service started becomes discoverable without a restart."""
    info_class, az_class = _patched_zeroconf(monkeypatch)
    svc = DiscoveryService(device_id=_DEVICE_ID)

    with patch.object(svc, "_local_addresses", return_value=[]):
        assert await svc.register(paired=False, code="123456") is False
    assert info_class.call_count == 0
    assert svc.registered is False

    with patch.object(svc, "_local_addresses", return_value=["192.168.4.1"]):
        assert await svc.refresh(paired=False, code="123456") is True
    assert svc.registered is True
    az_class.return_value.async_register_service.assert_awaited_once()


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

    with (
        patch.object(svc, "_local_addresses", return_value=["192.168.1.10"]),
        patch("ados.services.discovery.socket.gethostname", return_value="drone-rig"),
    ):
        await svc.register(paired=False, code="654321", profile="drone")

    assert info_class.call_count == 1
    args, kwargs = info_class.call_args
    assert args[0] == SERVICE_TYPE
    assert args[1] == f"ADOS-{_EXPECTED_SHORT}.{SERVICE_TYPE}"
    assert kwargs["port"] == 9090
    assert kwargs["addresses"] == [socket.inet_aton("192.168.1.10")]
    # `server` must be the RESOLVABLE system hostname, never a name derived
    # from the device id: publishing a service record with an invented
    # `server=` does not create a matching A/AAAA record, so the operator is
    # handed an address that does not resolve. The service INSTANCE name still
    # carries the short device id, which is what makes two nodes on one LAN
    # distinguishable.
    assert kwargs["server"] == "drone-rig.local."

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
    with patch.object(svc, "_local_addresses", return_value=["10.0.0.5"]):
        # Must not raise.
        await svc.register(paired=False, code="111111")

    # On failure the internal handles are cleared so a follow-up
    # refresh retries the registration and unregister is a no-op.
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
async def test_refresh_keeps_the_srv_target_on_the_resolvable_hostname(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Every refresh publishes the same resolvable SRV target as registration,
    never a constructed name nothing publishes an A-record for."""
    info_class, _ = _patched_zeroconf(monkeypatch)
    svc = DiscoveryService(device_id=_DEVICE_ID)

    with (
        patch.object(svc, "_local_addresses", return_value=["10.0.0.1"]),
        patch("ados.services.discovery.socket.gethostname", return_value="gs-rig"),
    ):
        await svc.register(paired=False, code="123456")
        await svc.refresh(paired=True, owner="owner-1")

    servers = [kwargs["server"] for _args, kwargs in info_class.call_args_list]
    assert servers == ["gs-rig.local.", "gs-rig.local."]


@pytest.mark.asyncio
async def test_refresh_swaps_in_paired_records(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """After pairing flips, the new ServiceInfo carries ``owner`` not ``code``."""
    info_class, az_class = _patched_zeroconf(monkeypatch)
    svc = DiscoveryService(device_id=_DEVICE_ID)

    with patch.object(svc, "_local_addresses", return_value=["10.0.0.1"]):
        await svc.register(paired=False, code="123456")

        info_class.reset_mock()

        await svc.refresh(paired=True, owner="owner-1", role="direct")

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
