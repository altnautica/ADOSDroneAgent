"""Tests for the TTL cache utility."""

from __future__ import annotations

import asyncio

import pytest

from ados.core.cache import TTLCache


@pytest.mark.asyncio
async def test_cache_hit_returns_cached_value():
    """Second call inside the TTL window must reuse the cached value."""
    cache = TTLCache()
    calls = 0

    async def fetch():
        nonlocal calls
        calls += 1
        return "value"

    result_a = await cache.get("k", fetch, ttl_seconds=10.0)
    result_b = await cache.get("k", fetch, ttl_seconds=10.0)

    assert result_a == "value"
    assert result_b == "value"
    assert calls == 1


@pytest.mark.asyncio
async def test_cache_miss_after_expiry():
    """A second call after TTL elapses must trigger a fresh fetch."""
    cache = TTLCache()
    calls = 0

    async def fetch():
        nonlocal calls
        calls += 1
        return calls

    first = await cache.get("k", fetch, ttl_seconds=0.05)
    await asyncio.sleep(0.1)
    second = await cache.get("k", fetch, ttl_seconds=0.05)

    assert first == 1
    assert second == 2
    assert calls == 2


@pytest.mark.asyncio
async def test_concurrent_miss_runs_fetch_once():
    """Concurrent gets on a missing key collapse onto a single fetch."""
    cache = TTLCache()
    calls = 0

    async def fetch():
        nonlocal calls
        calls += 1
        await asyncio.sleep(0.05)
        return "shared"

    results = await asyncio.gather(
        cache.get("k", fetch, ttl_seconds=10.0),
        cache.get("k", fetch, ttl_seconds=10.0),
        cache.get("k", fetch, ttl_seconds=10.0),
        cache.get("k", fetch, ttl_seconds=10.0),
        cache.get("k", fetch, ttl_seconds=10.0),
    )

    assert results == ["shared"] * 5
    assert calls == 1


@pytest.mark.asyncio
async def test_invalidate_forces_refetch():
    """Explicit invalidate must drop the entry and trigger a refetch."""
    cache = TTLCache()
    calls = 0

    async def fetch():
        nonlocal calls
        calls += 1
        return calls

    first = await cache.get("k", fetch, ttl_seconds=10.0)
    cache.invalidate("k")
    second = await cache.get("k", fetch, ttl_seconds=10.0)

    assert first == 1
    assert second == 2
    assert calls == 2


@pytest.mark.asyncio
async def test_invalidate_missing_key_is_noop():
    """invalidate on a key that was never set must not raise."""
    cache = TTLCache()
    cache.invalidate("never_set")


@pytest.mark.asyncio
async def test_invalidate_all_clears_every_entry():
    """invalidate_all drops every cached entry."""
    cache = TTLCache()
    calls_a = 0
    calls_b = 0

    async def fetch_a():
        nonlocal calls_a
        calls_a += 1
        return "a"

    async def fetch_b():
        nonlocal calls_b
        calls_b += 1
        return "b"

    await cache.get("a", fetch_a, ttl_seconds=10.0)
    await cache.get("b", fetch_b, ttl_seconds=10.0)
    cache.invalidate_all()
    await cache.get("a", fetch_a, ttl_seconds=10.0)
    await cache.get("b", fetch_b, ttl_seconds=10.0)

    assert calls_a == 2
    assert calls_b == 2


@pytest.mark.asyncio
async def test_independent_keys_do_not_collide():
    """Different keys must each carry their own value."""
    cache = TTLCache()

    async def fetch_a():
        return "alpha"

    async def fetch_b():
        return "beta"

    a = await cache.get("a", fetch_a, ttl_seconds=10.0)
    b = await cache.get("b", fetch_b, ttl_seconds=10.0)

    assert a == "alpha"
    assert b == "beta"
