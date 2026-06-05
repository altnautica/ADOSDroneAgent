"""Unit tests for the logd-sourced telemetry derivation (pure mapping)."""

from __future__ import annotations

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
