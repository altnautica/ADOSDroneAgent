"""Tests for the canBuses heartbeat enrichment.

The cloud subprocess folds an optional ``canBuses`` block into the
per-drone heartbeat by reading the on-disk parameter cache (default
``/var/lib/ados/params.json``). Mission Control consumes it to render
the per-port CAN driver / bitrate / protocol on the drone card.

These tests pin the contract that:

* No cache file present → empty result (field omitted from heartbeat).
* No CAN params in the cache → empty result (warmup window).
* Only one port configured → that single port is reported.
* Both ports configured → both rows present, integer-typed.
* Malformed cache file → empty result, no exception bubbling up.
"""

from __future__ import annotations

import json
import time
from pathlib import Path

from ados.services.cloud import heartbeat


def _write_cache(path: Path, params: dict[str, float]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    now = time.time()
    blob = {
        name: {"value": value, "param_type": 0, "last_updated": now}
        for name, value in params.items()
    }
    path.write_text(json.dumps(blob))


def test_can_buses_omitted_when_cache_missing(tmp_path: Path) -> None:
    """No on-disk param cache yet → no canBuses field in the heartbeat."""
    absent = tmp_path / "absent.json"
    out = heartbeat.build_can_buses_enrichment(absent)
    assert out == {}


def test_can_buses_omitted_when_no_can_params(tmp_path: Path) -> None:
    """Cache present but no CAN_* params → field omitted (warmup window)."""
    cache = tmp_path / "params.json"
    _write_cache(cache, {"AHRS_ORIENTATION": 0.0, "BATT_MONITOR": 4.0})
    out = heartbeat.build_can_buses_enrichment(cache)
    assert out == {}


def test_can_buses_single_port_configured(tmp_path: Path) -> None:
    """Port 1 configured for DroneCAN, port 2 absent → one row reported."""
    cache = tmp_path / "params.json"
    _write_cache(cache, {
        "CAN_P1_DRIVER": 1.0,
        "CAN_P1_BITRATE": 1_000_000.0,
        "CAN_D1_PROTOCOL": 1.0,
    })
    out = heartbeat.build_can_buses_enrichment(cache)
    assert out == {
        "canBuses": [
            {"port": 1, "driver": 1, "bitrate": 1_000_000, "protocol": 1},
        ]
    }


def test_can_buses_both_ports_reported(tmp_path: Path) -> None:
    """Both ports cached → both rows present, integer-typed."""
    cache = tmp_path / "params.json"
    _write_cache(cache, {
        "CAN_P1_DRIVER": 1.0,
        "CAN_P1_BITRATE": 1_000_000.0,
        "CAN_D1_PROTOCOL": 1.0,
        "CAN_P2_DRIVER": 0.0,
        "CAN_P2_BITRATE": 0.0,
        "CAN_D2_PROTOCOL": 0.0,
    })
    out = heartbeat.build_can_buses_enrichment(cache)
    assert out == {
        "canBuses": [
            {"port": 1, "driver": 1, "bitrate": 1_000_000, "protocol": 1},
            {"port": 2, "driver": 0, "bitrate": 0, "protocol": 0},
        ]
    }
    for entry in out["canBuses"]:
        for key in ("port", "driver", "bitrate", "protocol"):
            assert isinstance(entry[key], int)


def test_can_buses_partial_port_still_reported(tmp_path: Path) -> None:
    """Only one CAN param of a port cached → port shows with zero defaults."""
    cache = tmp_path / "params.json"
    _write_cache(cache, {"CAN_P1_DRIVER": 1.0})
    out = heartbeat.build_can_buses_enrichment(cache)
    assert out == {
        "canBuses": [
            {"port": 1, "driver": 1, "bitrate": 0, "protocol": 0},
        ]
    }


def test_can_buses_malformed_cache_does_not_raise(tmp_path: Path) -> None:
    """A garbage file on disk returns empty rather than blowing up the heartbeat."""
    cache = tmp_path / "params.json"
    cache.write_text("{not json at all")
    out = heartbeat.build_can_buses_enrichment(cache)
    assert out == {}


def test_can_buses_uses_default_path_when_unspecified(monkeypatch, tmp_path: Path) -> None:
    """Calling with no argument resolves the default cache path constant."""
    cache = tmp_path / "default.json"
    _write_cache(cache, {
        "CAN_P1_DRIVER": 1.0,
        "CAN_P1_BITRATE": 500_000.0,
        "CAN_D1_PROTOCOL": 1.0,
    })
    monkeypatch.setattr(heartbeat, "DEFAULT_PARAM_CACHE_PATH", str(cache))
    out = heartbeat.build_can_buses_enrichment()
    assert out == {
        "canBuses": [
            {"port": 1, "driver": 1, "bitrate": 500_000, "protocol": 1},
        ]
    }
