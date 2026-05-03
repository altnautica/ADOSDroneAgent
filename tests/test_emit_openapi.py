"""OpenAPI generator should mirror the live FastAPI route registration."""

from __future__ import annotations

from scripts.emit_openapi import build_spec_app


def test_emit_openapi_does_not_duplicate_v1_prefixes() -> None:
    spec = build_spec_app().openapi()
    paths = spec["paths"]

    assert not any("/api/v1/v1/" in path for path in paths)
    assert "/api/v1/ground-station/status" in paths
    assert "/api/v1/peripherals" in paths


def test_emit_openapi_includes_plugin_routes() -> None:
    spec = build_spec_app().openapi()
    paths = spec["paths"]

    assert "/api/plugins" in paths
    assert "/api/plugins/install" in paths
