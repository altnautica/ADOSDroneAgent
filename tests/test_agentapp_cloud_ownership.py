"""Regression tests for single-process AgentApp cloud ownership."""

from __future__ import annotations

from ados.core.config import ADOSConfig
from ados.core.main import AgentApp


def test_single_process_cloud_runtime_is_disabled_by_default() -> None:
    config = ADOSConfig()
    app = AgentApp(config)

    assert config.pairing.single_process_cloud_enabled is False
    assert app._single_process_cloud_enabled() is False


def test_single_process_cloud_runtime_can_be_enabled_explicitly() -> None:
    config = ADOSConfig()
    config.pairing.single_process_cloud_enabled = True
    app = AgentApp(config)

    assert app._single_process_cloud_enabled() is True
