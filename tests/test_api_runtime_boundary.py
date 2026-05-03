"""API runtime boundary should not depend on the legacy AgentApp module."""

from __future__ import annotations

from pathlib import Path


def test_standalone_api_service_does_not_import_legacy_agentapp() -> None:
    source = Path("src/ados/services/api/__main__.py").read_text()

    assert "from ados.core.main import" not in source
    assert "AgentApp-like" not in source
    assert "stand-in for AgentApp" not in source


def test_api_layer_does_not_type_against_legacy_agentapp() -> None:
    offenders = []
    for path in Path("src/ados/api").rglob("*.py"):
        source = path.read_text()
        if "ados.core.main" in source:
            offenders.append(str(path))

    assert offenders == []
