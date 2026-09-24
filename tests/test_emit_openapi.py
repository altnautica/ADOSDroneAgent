"""OpenAPI generator should mirror the live FastAPI route registration."""

from __future__ import annotations

from scripts.emit_openapi import build_spec_app


def test_emit_openapi_does_not_duplicate_v1_prefixes() -> None:
    spec = build_spec_app().openapi()
    paths = spec["paths"]

    assert not any("/api/v1/v1/" in path for path in paths)
    # Two live `/api/v1/` samples, one per sub-router, so the "no duplicated
    # prefix" assertion above cannot pass vacuously on an empty spec.
    # `ground-station/ui` used to be the first sample; it was deleted as a
    # dead route that `ados-control` already served natively, so the sample
    # moved rather than the route being resurrected to satisfy a test.
    assert "/api/v1/ground-station/factory-reset" in paths
    assert "/api/v1/peripherals" in paths
