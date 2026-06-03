"""Tests for ``ados network regulatory`` (operating-region posture CLI)."""

from __future__ import annotations

import json

import yaml
from click.testing import CliRunner

import ados.cli.network as netmod
from ados.cli.network import network_group
from ados.core.config import load_config


def _set_config_path(monkeypatch, tmp_path):
    cfg = tmp_path / "config.yaml"
    monkeypatch.setattr(netmod, "CONFIG_YAML", str(cfg))
    return cfg


def test_status_empty_config_is_unrestricted(monkeypatch, tmp_path) -> None:
    _set_config_path(monkeypatch, tmp_path)
    result = CliRunner().invoke(network_group, ["regulatory", "status"])
    assert result.exit_code == 0, result.output
    assert "unrestricted" in result.output.lower()


def test_pin_region_uppercases_and_persists(monkeypatch, tmp_path) -> None:
    cfg = _set_config_path(monkeypatch, tmp_path)
    runner = CliRunner()
    result = runner.invoke(network_group, ["regulatory", "region", "in"])
    assert result.exit_code == 0, result.output
    assert "IN" in result.output

    # status reflects the pin
    result = runner.invoke(network_group, ["regulatory", "status", "--json"])
    data = json.loads(result.output)
    assert data["mode"] == "region"
    assert data["region"] == "IN"
    assert data["ack_operator"] == "cli"
    assert data["ack_at"]

    # the written config round-trips through the pydantic loader
    loaded = load_config(str(cfg))
    assert loaded.network.regulatory.mode == "region"
    assert loaded.network.regulatory.region == "IN"


def test_unrestricted_clears_region(monkeypatch, tmp_path) -> None:
    cfg = _set_config_path(monkeypatch, tmp_path)
    runner = CliRunner()
    runner.invoke(network_group, ["regulatory", "region", "DE"])
    result = runner.invoke(network_group, ["regulatory", "unrestricted"])
    assert result.exit_code == 0, result.output
    loaded = load_config(str(cfg))
    assert loaded.network.regulatory.mode == "unrestricted"
    assert loaded.network.regulatory.region is None


def test_bad_region_rejected(monkeypatch, tmp_path) -> None:
    _set_config_path(monkeypatch, tmp_path)
    result = CliRunner().invoke(network_group, ["regulatory", "region", "USA"])
    assert result.exit_code != 0
    assert "2-letter" in result.output


def test_write_preserves_unrelated_keys(monkeypatch, tmp_path) -> None:
    cfg = _set_config_path(monkeypatch, tmp_path)
    cfg.write_text(yaml.safe_dump({"agent": {"name": "keepme"}}))
    CliRunner().invoke(network_group, ["regulatory", "region", "GB"])
    after = yaml.safe_load(cfg.read_text())
    assert after["agent"]["name"] == "keepme"
    assert after["network"]["regulatory"]["region"] == "GB"
