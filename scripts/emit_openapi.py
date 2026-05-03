"""Emit the agent's OpenAPI spec to a JSON file without booting services.

Run as:

    python scripts/emit_openapi.py [output_path]

Default output is `dist/openapi.json` next to the repo root. The GCS
build step consumes this file via openapi-typescript to regenerate
`src/lib/agent/api-types.gen.ts`.

This script avoids spinning up the AgentApp supervisor or any of the
runtime services. It registers each route module on a clean FastAPI
instance purely for spec introspection.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

# Allow `python scripts/emit_openapi.py` from the repo root without
# requiring an editable install.
ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from fastapi import FastAPI  # noqa: E402

from ados import __version__  # noqa: E402
from ados.api.routes import (  # noqa: E402
    commands,
    config,
    features,
    fleet,
    ground_station,
    logs,
    ota,
    pairing,
    params,
    peripherals,
    peripherals_v1,
    plugins,
    ros,
    scripts,
    services,
    signing,
    status,
    suites,
    system,
    version,
    video,
    wfb,
)


def build_spec_app() -> FastAPI:
    """Build a stripped FastAPI app with all routers registered."""
    app = FastAPI(
        title="ADOS Drone Agent",
        version=__version__,
        docs_url=None,
        redoc_url=None,
    )

    # Mirror the route registration order from src/ados/api/server.py.
    # Middlewares are intentionally left off; they do not affect the
    # OpenAPI surface.
    app.include_router(version.router, prefix="/api")
    app.include_router(status.router, prefix="/api")
    app.include_router(services.router, prefix="/api")
    app.include_router(params.router, prefix="/api")
    app.include_router(commands.router, prefix="/api")
    app.include_router(config.router, prefix="/api")
    app.include_router(logs.router, prefix="/api")
    app.include_router(video.router, prefix="/api")
    app.include_router(wfb.router, prefix="/api")
    app.include_router(scripts.router, prefix="/api")
    app.include_router(ota.router, prefix="/api")
    app.include_router(pairing.router, prefix="/api")
    app.include_router(system.router, prefix="/api")
    app.include_router(peripherals.router, prefix="/api")
    app.include_router(peripherals_v1.router, prefix="/api")
    app.include_router(suites.router, prefix="/api")
    app.include_router(fleet.router, prefix="/api")
    app.include_router(features.router, prefix="/api")
    app.include_router(ground_station.router, prefix="/api")
    app.include_router(ros.router, prefix="/api")
    app.include_router(signing.router, prefix="/api")
    app.include_router(plugins.router, prefix="/api")

    return app


def main(argv: list[str]) -> int:
    out_path = Path(argv[1]) if len(argv) > 1 else (ROOT / "dist" / "openapi.json")
    out_path.parent.mkdir(parents=True, exist_ok=True)

    app = build_spec_app()
    spec = app.openapi()
    out_path.write_text(json.dumps(spec, indent=2, sort_keys=True) + "\n")

    paths = spec.get("paths", {})
    components = spec.get("components", {}).get("schemas", {})
    print(
        f"openapi spec written to {out_path}: "
        f"{len(paths)} paths, {len(components)} schemas, "
        f"agent version {spec.get('info', {}).get('version', 'unknown')}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
