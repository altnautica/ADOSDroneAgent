"""Tests for config loading, validation, and defaults."""

from __future__ import annotations

import tempfile

import yaml

from ados.core.config import ADOSConfig, load_config


def test_default_config():
    """ADOSConfig with no args should have sensible defaults."""
    cfg = ADOSConfig()
    assert cfg.agent.name == "my-drone"
    assert cfg.mavlink.baud_rate == 57600
    assert cfg.mavlink.system_id == 1
    assert cfg.mavlink.component_id == 191
    assert cfg.logging.level == "info"
    assert cfg.swarm.enabled is False


def test_device_id_auto_generated():
    """Empty device_id should be auto-filled."""
    cfg = ADOSConfig()
    assert cfg.agent.device_id != ""
    assert len(cfg.agent.device_id) == 8


def test_load_config_from_yaml():
    """Config loaded from YAML should override defaults."""
    data = {
        "agent": {"name": "test-drone", "tier": "tier3"},
        "mavlink": {"baud_rate": 921600},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        yaml.dump(data, f)
        f.flush()
        cfg = load_config(f.name)

    assert cfg.agent.name == "test-drone"
    assert cfg.agent.tier == "tier3"
    assert cfg.mavlink.baud_rate == 921600
    # Defaults should still be intact
    assert cfg.logging.level == "info"


def test_load_config_no_file():
    """Loading from a non-existent path should return defaults."""
    cfg = load_config("/tmp/nonexistent-ados-config-12345.yaml")
    assert cfg.agent.name == "my-drone"


def test_config_extra_ignored():
    """Unknown keys in YAML should be silently ignored."""
    data = {
        "agent": {"name": "test"},
        "unknown_section": {"foo": "bar"},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".yaml", delete=False) as f:
        yaml.dump(data, f)
        f.flush()
        cfg = load_config(f.name)
    assert cfg.agent.name == "test"


def test_mavlink_endpoints_default():
    """Default endpoints should include one WebSocket on 8765."""
    cfg = ADOSConfig()
    assert len(cfg.mavlink.endpoints) >= 1
    assert cfg.mavlink.endpoints[0].type == "websocket"
    assert cfg.mavlink.endpoints[0].port == 8765


def test_security_defaults():
    """Security defaults should be reasonable."""
    cfg = ADOSConfig()
    assert cfg.security.tls.enabled is True
    assert cfg.security.api.cors_enabled is True
    assert "localhost:3000" in cfg.security.api.cors_origins[0]
