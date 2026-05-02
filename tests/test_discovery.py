"""Tests for mDNS discovery lifecycle behavior."""

from __future__ import annotations

import asyncio

import pytest

from ados.services.discovery import DiscoveryService


@pytest.mark.asyncio
async def test_unregister_awaits_unregister_broadcast_before_close() -> None:
    """Zeroconf returns a broadcast task that must settle before loop close."""
    events: list[str] = []

    async def unregister_broadcast() -> None:
        await asyncio.sleep(0)
        events.append("broadcast")

    class FakeZeroconf:
        async def async_unregister_service(self, info):
            events.append(f"unregister:{info}")
            return asyncio.create_task(unregister_broadcast())

        async def async_close(self):
            events.append("close")

    service = DiscoveryService(device_id="abcdef123456")
    service._zeroconf = FakeZeroconf()
    service._info = "service-info"

    await service.unregister()

    assert events == ["unregister:service-info", "broadcast", "close"]
    assert service._zeroconf is None
    assert service._info is None
