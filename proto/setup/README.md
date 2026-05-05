# proto/setup/ — setup webapp REST contract

Documents the REST surface served by the agent at `/api/v1/setup/*` and consumed by the universal setup webapp at `web/setup/`.

## Files

| File | Purpose | Status |
|---|---|---|
| `setup-api.yaml` | OpenAPI spec for the 12 setup REST routes | TODO |

## Source of truth

The Python full agent at `src/ados/api/routes/setup.py` is the reference implementation. The OpenAPI spec is auto-generated from FastAPI's runtime introspection and committed here as the canonical surface. Alternate backends serve the same routes with the same response shapes.

## Authentication

Setup mutations follow same-origin trust by default and require an `X-ADOS-Setup-Token` header when `config.security.setup_token_required` is true. Cross-origin callers (cloud relay, third-party integrations) require `X-ADOS-Key`.
