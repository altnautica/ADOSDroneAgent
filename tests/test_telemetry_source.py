"""Unit tests for the shared logd read client and its telemetry derivation."""

from __future__ import annotations

import httpx
import pytest

from ados.api import telemetry_source
from ados.api.telemetry_source import derive_resources

_MB = 1024 * 1024
_GB = 1024 * 1024 * 1024


def _signals(**overrides):
    base = {
        "mem.total_bytes": 4_000_000_000,
        "mem.avail_bytes": 1_000_000_000,
        "mem.cache_bytes": 500_000_000,
        "mem.swap_total_bytes": 1_000_000_000,
        "mem.swap_free_bytes": 800_000_000,
        "cpu.util.all": 42.5,
        "disk.fs_total_bytes": 32_000_000_000,
        "disk.fs_used_bytes": 8_000_000_000,
        "thermal.primary_c": 48.0,
        "thermal.cpu_thermal_c": 48.0,
        "thermal.hwmon.rpi_volt_temp1_c": 50.0,
        "sched.loadavg_1": 0.5,
        "sched.loadavg_5": 0.4,
        "sched.loadavg_15": 0.3,
    }
    base.update(overrides)
    return base


def test_derive_resources_maps_every_field():
    r = derive_resources(_signals())
    assert r is not None
    assert r["cpu_percent"] == 42.5
    assert r["memory_total_mb"] == round(4_000_000_000 / _MB)
    assert r["memory_used_mb"] == round(3_000_000_000 / _MB)
    assert r["memory_available_mb"] == round(1_000_000_000 / _MB)
    assert r["memory_cache_mb"] == round(500_000_000 / _MB)
    # swap used = total - free.
    assert r["swap_total_mb"] == round(1_000_000_000 / _MB)
    assert r["swap_used_mb"] == round(200_000_000 / _MB)
    assert r["disk_total_gb"] == round(32_000_000_000 / _GB, 1)
    assert r["disk_used_gb"] == round(8_000_000_000 / _GB, 1)
    assert r["temperature"] == 48.0
    assert r["load_avg"] == [0.5, 0.4, 0.3]


def test_temperatures_map_excludes_primary_and_keeps_sensor_names():
    temps = derive_resources(_signals())["temperatures"]
    assert "cpu_thermal" in temps
    assert "hwmon.rpi_volt_temp1" in temps
    # the primary is surfaced separately, not as a duplicate sensor entry.
    assert "primary" not in temps


def test_none_when_an_essential_field_is_missing():
    for missing in (
        "mem.total_bytes",
        "mem.avail_bytes",
        "cpu.util.all",
        "disk.fs_total_bytes",
        "disk.fs_used_bytes",
    ):
        s = _signals()
        del s[missing]
        assert derive_resources(s) is None, f"{missing} should be essential"


def test_zero_swap_does_not_divide_by_zero():
    r = derive_resources(_signals(**{"mem.swap_total_bytes": 0, "mem.swap_free_bytes": 0}))
    assert r is not None
    assert r["swap_total_mb"] == 0
    assert r["swap_percent"] == 0.0


def test_booleans_are_not_treated_as_measurements():
    # A boolean-valued signal must not satisfy an essential numeric field.
    s = _signals(**{"cpu.util.all": True})
    assert derive_resources(s) is None


# ---------------------------------------------------------------------------
# The shared upstream client.
#
# This coverage used to sit in a per-domain source's test, which is where it
# happened to be needed rather than where it belongs. Those domain modules were
# store-read reconstructors for routes that have since migrated to the native
# front; when the last of them was deleted, the only test driving this seam went
# with it. It lives here now, next to the client itself, so it survives the next
# domain module being retired.
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _reset_shared_client():
    """Each test starts from no cached client and leaves none behind."""
    telemetry_source._client = None
    yield
    telemetry_source._client = None


def test_get_client_is_cached_across_calls() -> None:
    """One connection to the store, not one per read.

    The module docstring's contract is "one connection, one timeout policy, one
    gap-tolerance contract". A client built per call would still work and would
    silently open a socket per request.
    """
    first = telemetry_source._get_client()
    second = telemetry_source._get_client()
    assert first is second


def test_get_client_talks_over_the_logd_unix_socket() -> None:
    """The transport is the on-box UDS, not a TCP host.

    The store is reachable over a unix socket precisely so a read works when the
    network is down; a client built against a TCP base URL would appear healthy
    in tests and fail on a box with no route to itself.
    """
    client = telemetry_source._get_client()
    assert isinstance(client, httpx.AsyncClient)
    transport = client._transport
    assert isinstance(transport, httpx.AsyncHTTPTransport), transport
    # The uds path is held on the underlying connection pool.
    pool = transport._pool
    assert getattr(pool, "_uds", None) == str(telemetry_source.LOGD_QUERY_SOCK), (
        f"expected the logd query socket, got {getattr(pool, '_uds', None)!r}"
    )


@pytest.mark.asyncio
async def test_aclose_drops_the_cached_client() -> None:
    """Shutdown must release it, and the next read must rebuild rather than
    reuse a closed client."""
    first = telemetry_source._get_client()
    await telemetry_source.aclose()
    assert telemetry_source._client is None
    assert telemetry_source._get_client() is not first
