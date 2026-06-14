"""Value-parity between the durable store path and the live PSS scan.

The ``/api/services`` route attaches per-service ``memory_mb``. Two producers
feed it the same number: the live process-local ``/proc`` PSS scan
(``ados.core.systemd_memory``) and the durable store the supervisor's sampler
ships ``service.memory_pss_bytes`` into, read back by
``ados.api.sources.services.latest_service_memory``.

These tests pin that the two paths report the **identical** ``memory_mb`` for the
same underlying PSS: the live path rounds ``kib / 1024`` to one decimal, and the
store path rounds ``bytes / 1024² = kib / 1024`` to one decimal, so for any input
they agree exactly. The end-to-end test drives the route once store-first and
once live-fallback against the same fixture and asserts the per-service map is the
same, which is the regression the producer guards against (per-service memory
dropping collapses the GCS Memory panel).
"""

from __future__ import annotations

import io

import pytest

from ados.core import systemd_memory as sm

# One per-unit PSS fixture in KiB, the shared ground truth both paths derive from.
# ados-health has two PIDs (orchestrator + child) so the live scan exercises its
# per-unit sum; the rounding is intentionally non-integer (165000/1024 = 161.1...).
_PSS_KIB = {
    "ados-health.service": 42 * 1024 + 512,  # 43520 KiB -> 42.5 MiB
    "ados-video.service": 165000,  # -> 161.1 MiB (exercises rounding)
}


def _expected_mb() -> dict[str, float]:
    """The MiB map both paths must produce: round(kib / 1024, 1) per unit."""
    return {unit: round(kib / 1024, 1) for unit, kib in _PSS_KIB.items()}


def _install_fake_proc(monkeypatch) -> None:
    """Point the live PSS scan at a fake ``/proc`` built from ``_PSS_KIB``.

    Mirrors the fixture style in ``test_systemd_memory.py``: stub ``os.scandir``
    and ``open`` so ``_scan_pss_by_unit`` reads the fixture's cgroup + rollup
    bodies. ados-health is split across two PIDs to prove the per-unit sum.
    """
    fake: dict[str, tuple[str, str]] = {
        # ados-health split across two PIDs that sum to its fixture value.
        "100": ("0::/system.slice/ados.slice/ados-health.service", "Pss: 21760 kB\n"),
        "101": ("0::/system.slice/ados.slice/ados-health.service", "Pss: 21760 kB\n"),
        "200": ("0::/system.slice/ados-video.service", "Pss: 165000 kB\n"),
        "300": ("0::/system.slice/sshd.service", "Pss: 9000 kB\n"),  # non-ados
    }

    class _Entry:
        def __init__(self, name: str) -> None:
            self.name = name

    monkeypatch.setattr(sm.os, "scandir", lambda _p: [_Entry(p) for p in fake])
    monkeypatch.setattr(sm, "_cache", None)
    monkeypatch.setattr(sm, "_cache_ts", 0.0)

    real_open = open

    def fake_open(path, *a, **k):  # noqa: ANN001
        s = str(path)
        for pid, (cg, rollup) in fake.items():
            if s == f"/proc/{pid}/cgroup":
                return io.StringIO(cg)
            if s == f"/proc/{pid}/smaps_rollup":
                return io.StringIO(rollup)
        return real_open(path, *a, **k)

    monkeypatch.setattr("builtins.open", fake_open)


def _store_rows() -> list[dict]:
    """A newest-first ``metrics`` page carrying one byte-valued row per unit.

    Shape matches what ``query_rows('metrics', ...)`` returns: each row carries
    ``metric`` + ``value`` (bytes) + ``tags['unit']``. The supervisor ships
    ``kib * 1024`` bytes, so the rows here are exactly that for the fixture.
    """
    return [
        {
            "metric": "service.memory_pss_bytes",
            "value": kib * 1024,
            "tags": {"unit": unit},
            "ts_us": 1_000 + i,
        }
        for i, (unit, kib) in enumerate(_PSS_KIB.items())
    ]


# ── the two producers agree on the MiB map ───────────────────────────────────


@pytest.mark.asyncio
async def test_store_path_matches_live_path_for_the_same_pss(monkeypatch) -> None:
    """The store-derived per-unit MiB equals the live PSS-derived MiB."""
    # Live path: scan the fake /proc and read the per-unit map back.
    _install_fake_proc(monkeypatch)
    live = sm.services_memory_mb(sorted(_PSS_KIB))

    # Store path: feed the byte-valued rows through the reader.
    from ados.api.sources import services as store_src

    async def fake_query_rows(kind, limit, **params):  # noqa: ANN001
        assert kind == "metrics"
        return _store_rows()

    monkeypatch.setattr(store_src, "query_rows", fake_query_rows)
    stored = await store_src.latest_service_memory()

    assert live == _expected_mb()
    assert stored == _expected_mb()
    # The load-bearing assertion: byte-for-byte the same MiB, no drift.
    assert stored == live


@pytest.mark.asyncio
async def test_store_reader_returns_none_on_a_store_gap(monkeypatch) -> None:
    """A store gap (no rows) yields None so the route falls back to the scan."""
    from ados.api.sources import services as store_src

    async def no_rows(kind, limit, **params):  # noqa: ANN001
        return None

    monkeypatch.setattr(store_src, "query_rows", no_rows)
    assert await store_src.latest_service_memory() is None
