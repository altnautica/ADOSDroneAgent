"""Interface-named MAC overrides are rekeyed to the adapter they belonged to.

The reconciler keys an override by ``<vidpid>@<usb_path>`` (or a bare
``vidpid``); an override keyed by an interface name is ignored at runtime,
because the name can flip between boots. The pin state file records which
adapter each interface name was, so the upgrade pass can carry the operator's
override over instead of silently dropping it.
"""

from __future__ import annotations

import json
from pathlib import Path

import yaml

from ados.core.config import _migrators, load_config
from ados.core.config.maintenance import migrate_config_file

_MAC = "02:11:22:33:44:55"
_MODEL_MAC = "02:66:77:88:99:aa"


def _state(tmp_path: Path, monkeypatch, adapters: list[dict]) -> None:
    state = tmp_path / "mac-pins.state"
    state.write_text(json.dumps({"version": 1, "adapters": adapters, "learner": []}))
    monkeypatch.setattr(_migrators, "MAC_PINS_STATE_PATH", state)


def _raw(overrides: dict[str, str]) -> dict:
    return {"network": {"mac_pin": {"enabled": True, "overrides": dict(overrides)}}}


def test_an_interface_named_override_moves_to_its_adapter_key(tmp_path, monkeypatch):
    _state(
        tmp_path,
        monkeypatch,
        [{"name": "wlan1", "vidpid": "0bda:c811", "usb_path": "1-1.3", "state": "pinned"}],
    )
    raw = _raw({"wlan1": _MAC, "0bda:a81a": _MODEL_MAC})

    assert _migrators.apply_mac_pin_overrides_to_adapter_keys(raw) is True
    assert raw["network"]["mac_pin"]["overrides"] == {
        "0bda:c811@1-1.3": _MAC,
        "0bda:a81a": _MODEL_MAC,
    }
    # Applying it again finds nothing left to move.
    assert _migrators.apply_mac_pin_overrides_to_adapter_keys(raw) is False


def test_an_unresolvable_name_is_left_alone(tmp_path, monkeypatch):
    _state(
        tmp_path,
        monkeypatch,
        [
            {"name": "wlan0", "vidpid": "0bda:c811", "usb_path": "1-1.2", "state": "stable"},
            # A platform NIC has no USB identity to move an override onto.
            {"name": "wlan1", "vidpid": "", "usb_path": "", "state": "stable"},
        ],
    )
    raw = _raw({"wlan1": _MAC})

    assert _migrators.apply_mac_pin_overrides_to_adapter_keys(raw) is False
    assert raw["network"]["mac_pin"]["overrides"] == {"wlan1": _MAC}


def test_no_state_file_changes_nothing(tmp_path, monkeypatch):
    monkeypatch.setattr(_migrators, "MAC_PINS_STATE_PATH", tmp_path / "absent.state")
    raw = _raw({"wlan1": _MAC})

    assert _migrators.apply_mac_pin_overrides_to_adapter_keys(raw) is False
    assert raw["network"]["mac_pin"]["overrides"] == {"wlan1": _MAC}


def test_an_existing_adapter_key_wins_over_the_interface_named_one(tmp_path, monkeypatch):
    _state(
        tmp_path,
        monkeypatch,
        [{"name": "wlan1", "vidpid": "0bda:c811", "usb_path": "1-1.3", "state": "pinned"}],
    )
    raw = _raw({"wlan1": _MAC, "0bda:c811@1-1.3": _MODEL_MAC})

    assert _migrators.apply_mac_pin_overrides_to_adapter_keys(raw) is False
    assert raw["network"]["mac_pin"]["overrides"] == {
        "wlan1": _MAC,
        "0bda:c811@1-1.3": _MODEL_MAC,
    }


def test_the_upgrade_pass_persists_the_rekeyed_override_once(tmp_path, monkeypatch):
    _state(
        tmp_path,
        monkeypatch,
        [{"name": "wlan1", "vidpid": "0bda:c811", "usb_path": "1-1.3", "state": "pinned"}],
    )
    monkeypatch.setattr(
        "ados.core.config._migrators._LEGACY_GS_UI_PATH", tmp_path / "absent.json"
    )
    cfg = tmp_path / "config.yaml"
    cfg.write_text(yaml.safe_dump(_raw({"wlan1": _MAC}), sort_keys=False))
    ledger = tmp_path / "config-migrations.json"

    first = migrate_config_file(cfg, ledger_path=ledger)

    assert "mac_pin_overrides_adapter_keys" in first.applied
    assert load_config(cfg).network.mac_pin.overrides == {"0bda:c811@1-1.3": _MAC}

    # An interface-named key written after the upgrade is the operator's own.
    raw = yaml.safe_load(cfg.read_text())
    raw["network"]["mac_pin"]["overrides"]["wlan1"] = _MODEL_MAC
    cfg.write_text(yaml.safe_dump(raw, sort_keys=False))

    second = migrate_config_file(cfg, ledger_path=ledger)

    assert "mac_pin_overrides_adapter_keys" not in second.applied
    assert yaml.safe_load(cfg.read_text())["network"]["mac_pin"]["overrides"]["wlan1"] == _MODEL_MAC
