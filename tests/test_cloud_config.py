"""The cloud server block declares only keys a service reads."""

from __future__ import annotations

from pathlib import Path

import yaml

from ados.core.config import SECRET_PATHS, ServerConfig

_DEFAULTS = Path(__file__).resolve().parents[1] / "src" / "ados" / "core" / "defaults.yaml"
_DEAD = ("mqtt_username", "mqtt_password", "mqtt_plaintext_dev")


def test_server_config_has_no_dead_mqtt_keys() -> None:
    """The relay authenticates as ``ados-<device_id>`` with the pairing key over
    TLS; nothing reads a configured MQTT username, password or plaintext switch,
    so none of them is offered as a setting."""
    fields = set(ServerConfig.model_fields)
    for key in _DEAD:
        assert key not in fields
        assert f"server.{key}" not in SECRET_PATHS
    server = yaml.safe_load(_DEFAULTS.read_text(encoding="utf-8"))["server"]
    for key in _DEAD:
        assert key not in server


def test_a_file_that_still_carries_them_loads() -> None:
    """Unknown keys are ignored, so a node configured by an older installer
    keeps loading."""
    config = ServerConfig(mqtt_username="ados", mqtt_password="x", mqtt_plaintext_dev=True)
    assert not hasattr(config, "mqtt_password")
