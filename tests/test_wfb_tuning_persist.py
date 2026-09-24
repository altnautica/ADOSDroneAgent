"""Tests for runtime radio-tuning persistence and link-preset routing.

Covers the net-new logic behind POST /api/video/config tuning:
  * `_persist_wfb_fields` merges updates into the on-disk video.wfb block.
  * `_PRESET_TRIOS` matches the radio's preset table.
  * A knob the radio did not apply is named in ``warnings`` and never persisted.
"""

from __future__ import annotations

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


def test_preset_trios_match_the_radio_table() -> None:
    # Byte-identical to crates/ados-radio/src/config.rs link_preset_trio.
    assert ec._PRESET_TRIOS == {
        "conservative": (1, 8, 12),
        "balanced": (3, 8, 12),
        "aggressive": (5, 8, 10),
    }


def test_unapplied_knob_is_reported_and_not_persisted(tmp_path, monkeypatch) -> None:
    """With the radio's command socket absent, an MCS change cannot have
    reached the air. It must say so, and must not be written to config where
    it would read as the operator's applied setting."""
    from fastapi.testclient import TestClient

    from ados.api.server import create_app
    from ados.services.wfb import cmd_client
    from tests.api_runtime_utils import build_api_runtime

    cfg = tmp_path / "config.yaml"
    monkeypatch.setattr(wfb_routes, "CONFIG_YAML", cfg)
    monkeypatch.setattr(cmd_client, "WFB_CMD_SOCK", tmp_path / "absent.sock")
    client = TestClient(create_app(build_api_runtime()))
    resp = client.post("/api/video/config", json={"mcs": 4, "bitrate_kbps": 3000})
    assert resp.status_code == 200
    warnings = resp.json()["warnings"]
    assert "radio_unavailable" in warnings
    assert "bitrate_not_settable_on_this_surface" in warnings
    assert not cfg.exists()
