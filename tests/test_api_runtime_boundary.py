"""API service should not depend on the legacy AgentApp module."""

from __future__ import annotations

from pathlib import Path


def test_standalone_api_service_does_not_import_legacy_agentapp() -> None:
    source = Path("src/ados/services/api/__main__.py").read_text()

    assert "from ados.core.main import" not in source
    assert "AgentApp-like" not in source
    assert "stand-in for AgentApp" not in source
