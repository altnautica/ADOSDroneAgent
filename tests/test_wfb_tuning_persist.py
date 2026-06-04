"""Tests for runtime radio-tuning persistence and link-preset routing.

Covers the net-new logic behind POST /api/video/config tuning:
  * `_persist_wfb_fields` merges updates into the on-disk video.wfb block.
  * `_PRESET_TRIOS` matches the radio's preset table.
  * `_apply_preset` resolves the trio and drives the packaged manager when the
    native radio socket is not the active transmit plane.
"""

from __future__ import annotations

import pytest
import yaml

from ados.api.routes import wfb as wfb_routes
from ados.api.routes.video import encoder_config as ec


def test_persist_wfb_fields_creates_block(tmp_path, monkeypatch) -> None:
    cfg = tmp_path / "config.yaml"
    monkeypatch.setattr(wfb_routes, "CONFIG_YAML", cfg)
    assert wfb_routes._persist_wfb_fields({"fec_k": 8, "fec_n": 14})
    data = yaml.safe_load(cfg.read_text())
    assert data["video"]["wfb"]["fec_k"] == 8
    assert data["video"]["wfb"]["fec_n"] == 14


def test_persist_wfb_fields_merges_without_clobbering(tmp_path, monkeypatch) -> None:
    cfg = tmp_path / "config.yaml"
    cfg.write_text(
        yaml.safe_dump({"video": {"wfb": {"channel": 149, "fec_k": 8}}})
    )
    monkeypatch.setattr(wfb_routes, "CONFIG_YAML", cfg)
    assert wfb_routes._persist_wfb_fields({"mcs_index": 3, "fec_n": 16})
    wfb = yaml.safe_load(cfg.read_text())["video"]["wfb"]
    # Pre-existing keys survive; new keys land alongside them.
    assert wfb["channel"] == 149
    assert wfb["fec_k"] == 8
    assert wfb["mcs_index"] == 3
    assert wfb["fec_n"] == 16


def test_persist_tx_power_is_a_thin_wrapper(tmp_path, monkeypatch) -> None:
    cfg = tmp_path / "config.yaml"
    monkeypatch.setattr(wfb_routes, "CONFIG_YAML", cfg)
    assert wfb_routes._persist_tx_power(9)
    assert yaml.safe_load(cfg.read_text())["video"]["wfb"]["tx_power_dbm"] == 9


def test_preset_trios_match_the_radio_table() -> None:
    # Byte-identical to crates/ados-radio/src/config.rs link_preset_trio.
    assert ec._PRESET_TRIOS == {
        "conservative": (1, 8, 12),
        "balanced": (3, 8, 12),
        "aggressive": (5, 8, 10),
    }


class _FakeManager:
    """Minimal packaged wfb manager: records the trio it was driven with."""

    def __init__(self) -> None:
        self.mcs: int | None = None
        self.fec: tuple[int, int] | None = None

    async def set_mcs(self, mcs: int) -> bool:
        self.mcs = mcs
        return True

    async def set_fec(self, k: int, n: int) -> bool:
        self.fec = (k, n)
        return True


@pytest.mark.asyncio
async def test_apply_preset_packaged_path_resolves_trio() -> None:
    mgr = _FakeManager()
    warnings: list[str] = []
    trio = await ec._apply_preset(False, mgr, "balanced", warnings)
    assert trio == (3, 8, 12)
    assert mgr.mcs == 3
    assert mgr.fec == (8, 12)
    assert warnings == []


@pytest.mark.asyncio
async def test_apply_preset_without_manager_warns() -> None:
    warnings: list[str] = []
    trio = await ec._apply_preset(False, None, "aggressive", warnings)
    assert trio is None
    assert "wfb_manager_not_in_process" in warnings
