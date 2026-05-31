"""Tests for per-service memory readback (ados.core.systemd_memory).

The implementation reads PSS from /proc grouped by each process's systemd
cgroup (the kernel memory cgroup controller is off by default on Raspberry
Pi, so systemd MemoryCurrent is unusable there). These tests cover the pure
parsers and the batch/lookup surface with the /proc scan stubbed out.
"""

from __future__ import annotations

import pytest

from ados.core import systemd_memory as sm

# ── unit_from_cgroup ─────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("body", "expected"),
    [
        ("0::/system.slice/ados.slice/ados-video.service", "ados-video.service"),
        ("0::/system.slice/ados-api.service", "ados-api.service"),
        ("0::/system.slice/ados.slice/ados-wfb-rx.service\n", "ados-wfb-rx.service"),
        # v1-style multi-line cgroup file still matches the unit token.
        ("12:pids:/system.slice/ados-cloud.service\n0::/system.slice/ados-cloud.service", "ados-cloud.service"),
        ("0::/system.slice/sshd.service", None),
        ("0::/user.slice/user-1000.slice", None),
        ("", None),
    ],
)
def test_unit_from_cgroup(body: str, expected: str | None) -> None:
    assert sm.unit_from_cgroup(body) == expected


# ── pss_kib_from_rollup ──────────────────────────────────────────────────────


def test_pss_kib_from_rollup_parses_pss_line() -> None:
    rollup = (
        "55a0..-55a1.. ---p 00000000 00:00 0 [rollup]\n"
        "Rss:              180000 kB\n"
        "Pss:              165000 kB\n"
        "Shared_Clean:      12000 kB\n"
    )
    assert sm.pss_kib_from_rollup(rollup) == 165000


@pytest.mark.parametrize("body", ["", "Rss: 1000 kB\n", "Pss: not-a-number kB\n", "Pss:\n"])
def test_pss_kib_from_rollup_zero_when_absent_or_malformed(body: str) -> None:
    assert sm.pss_kib_from_rollup(body) == 0


# ── services_memory_mb / service_memory_mb (scan stubbed) ────────────────────


def test_services_memory_mb_maps_units_and_defaults_missing(monkeypatch) -> None:
    monkeypatch.setattr(
        sm, "_pss_map", lambda: {"ados-video.service": 165.0, "ados-api.service": 79.2}
    )
    out = sm.services_memory_mb(
        ["ados-video.service", "ados-api.service", "ados-wfb.service"]
    )
    assert out == {
        "ados-video.service": 165.0,
        "ados-api.service": 79.2,
        "ados-wfb.service": 0.0,  # not running / unknown -> 0, never dropped
    }


def test_service_memory_mb_lookup(monkeypatch) -> None:
    monkeypatch.setattr(sm, "_pss_map", lambda: {"ados-cloud.service": 3.0})
    assert sm.service_memory_mb("ados-cloud.service") == 3.0
    assert sm.service_memory_mb("ados-missing.service") == 0.0


# ── unit_for_service resolver ────────────────────────────────────────────────


@pytest.mark.parametrize(
    ("name", "expected"),
    [
        ("ados-video", "ados-video.service"),
        ("ados-api.service", "ados-api.service"),
        ("video-pipeline", "ados-video.service"),
        ("rest-api", "ados-api.service"),
        ("mavlink-ws-proxy", None),
        ("", None),
    ],
)
def test_unit_for_service(name: str, expected: str | None) -> None:
    assert sm.unit_for_service(name) == expected


# ── _scan_pss_by_unit groups + sums children (real /proc-shaped fakes) ───────


def test_scan_groups_and_sums_by_unit(monkeypatch, tmp_path) -> None:
    """A unit with two PIDs (e.g. orchestrator + ffmpeg child) sums; PSS in MiB."""
    fake = {
        "100": ("0::/system.slice/ados.slice/ados-video.service", "Pss: 10240 kB\n"),
        "101": ("0::/system.slice/ados.slice/ados-video.service", "Pss: 153600 kB\n"),
        "200": ("0::/system.slice/ados-api.service", "Pss: 81100 kB\n"),
        "300": ("0::/system.slice/sshd.service", "Pss: 9000 kB\n"),  # non-ados, skipped
    }

    class _Entry:
        def __init__(self, name: str) -> None:
            self.name = name

    monkeypatch.setattr(sm.os, "scandir", lambda _p: [_Entry(p) for p in fake])

    real_open = open

    def fake_open(path, *a, **k):  # noqa: ANN001
        s = str(path)
        for pid, (cg, rollup) in fake.items():
            if s == f"/proc/{pid}/cgroup":
                import io

                return io.StringIO(cg)
            if s == f"/proc/{pid}/smaps_rollup":
                import io

                return io.StringIO(rollup)
        return real_open(path, *a, **k)

    monkeypatch.setattr("builtins.open", fake_open)
    out = sm._scan_pss_by_unit()
    assert out["ados-video.service"] == 160.0  # (10240+153600)/1024
    assert out["ados-api.service"] == round(81100 / 1024, 1)
    assert "sshd.service" not in out
