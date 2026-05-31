"""Tests for per-service memory readback (ados.core.systemd_memory)."""

from __future__ import annotations

import subprocess

import pytest

from ados.core import systemd_memory as sm

# ── parse logic ─────────────────────────────────────────────────────────────

@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("0", 0.0),
        ("1048576", 1.0),               # exactly 1 MiB
        ("173015040", 165.0),           # ados-video ballpark, ~165 MiB
        ("82837504", 79.0),             # ados-api ballpark, ~79 MiB
        ("1572864", 1.5),               # a Rust service, ~1.5 MiB
        ("  2097152  ", 2.0),           # whitespace tolerated
    ],
)
def test_parse_memory_current_bytes_to_mib(raw: str, expected: float) -> None:
    assert sm._parse_memory_current(raw) == expected


@pytest.mark.parametrize(
    "raw",
    [
        "",                              # empty
        "[not set]",                     # unit not running
        "18446744073709551615",          # u64 max sentinel (accounting off)
        "garbage",                       # non-numeric
        "-1",                            # negative is nonsense
    ],
)
def test_parse_memory_current_sentinels_and_errors_are_zero(raw: str) -> None:
    assert sm._parse_memory_current(raw) == 0.0


# ── service_memory_mb (monkeypatched systemctl) ─────────────────────────────

def _fake_run(stdout: str, returncode: int = 0):
    def run(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        return subprocess.CompletedProcess(
            args=cmd, returncode=returncode, stdout=stdout, stderr="",
        )
    return run


def test_service_memory_mb_parses_systemctl_bytes(monkeypatch) -> None:
    monkeypatch.setattr(sm.subprocess, "run", _fake_run("173015040\n"))
    assert sm.service_memory_mb("ados-video.service") == 165.0


def test_service_memory_mb_not_set_is_zero(monkeypatch) -> None:
    monkeypatch.setattr(sm.subprocess, "run", _fake_run("[not set]\n"))
    assert sm.service_memory_mb("ados-wfb.service") == 0.0


def test_service_memory_mb_max_sentinel_is_zero(monkeypatch) -> None:
    monkeypatch.setattr(
        sm.subprocess, "run", _fake_run("18446744073709551615\n")
    )
    assert sm.service_memory_mb("ados-oled.service") == 0.0


def test_service_memory_mb_nonzero_returncode_is_zero(monkeypatch) -> None:
    monkeypatch.setattr(sm.subprocess, "run", _fake_run("123\n", returncode=1))
    assert sm.service_memory_mb("ados-nope.service") == 0.0


def test_service_memory_mb_subprocess_error_is_zero(monkeypatch) -> None:
    def boom(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        raise OSError("systemctl not found")

    monkeypatch.setattr(sm.subprocess, "run", boom)
    assert sm.service_memory_mb("ados-api.service") == 0.0


def test_service_memory_mb_timeout_is_zero(monkeypatch) -> None:
    def slow(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        raise subprocess.TimeoutExpired(cmd=cmd, timeout=5.0)

    monkeypatch.setattr(sm.subprocess, "run", slow)
    assert sm.service_memory_mb("ados-api.service") == 0.0


# ── batch helper ────────────────────────────────────────────────────────────

def test_services_memory_mb_batches_each_unit(monkeypatch) -> None:
    seen: list[str] = []

    def run(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        unit = cmd[2]  # systemctl show <unit> -p MemoryCurrent --value
        seen.append(unit)
        # Return a distinct value keyed off the unit name length so we
        # can confirm the mapping is per-unit, not a single value reused.
        value = str(len(unit) * 1048576)
        return subprocess.CompletedProcess(
            args=cmd, returncode=0, stdout=value, stderr="",
        )

    monkeypatch.setattr(sm.subprocess, "run", run)
    units = ["ados-api.service", "ados-video.service"]
    result = sm.services_memory_mb(units)
    assert set(result) == set(units)
    assert result["ados-api.service"] == float(len("ados-api.service"))
    assert result["ados-video.service"] == float(len("ados-video.service"))
    assert seen == units  # one probe per unit, in order


def test_services_memory_mb_empty_input(monkeypatch) -> None:
    # No subprocess should be spawned for an empty unit list.
    def boom(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        raise AssertionError("systemctl should not be called for empty input")

    monkeypatch.setattr(sm.subprocess, "run", boom)
    assert sm.services_memory_mb([]) == {}


# ── unit_for_service resolver ───────────────────────────────────────────────

def test_unit_for_service_unit_basename_gets_service_suffix() -> None:
    # systemd-fallback entries already carry the unit basename.
    assert sm.unit_for_service("ados-video") == "ados-video.service"
    # An already-suffixed name is returned unchanged.
    assert sm.unit_for_service("ados-api.service") == "ados-api.service"


def test_unit_for_service_short_names_map_through_table() -> None:
    assert sm.unit_for_service("fc-connection") == "ados-mavlink.service"
    assert sm.unit_for_service("video-pipeline") == "ados-video.service"
    assert sm.unit_for_service("rest-api") == "ados-api.service"


def test_unit_for_service_unknown_or_empty_is_none() -> None:
    assert sm.unit_for_service("mavlink-ws-proxy") is None
    assert sm.unit_for_service("") is None
