"""Emit the JSON Schemas of the agent's Python-served REST responses.

Run as:

    python scripts/emit_api_schemas.py            # (re)write the schema files
    python scripts/emit_api_schemas.py --check    # verify, non-zero on drift

Writes ``schemas/api/setup-status.schema.json`` from the ``SetupStatus``
Pydantic model behind ``GET /api/v1/setup/status``. The committed file is one
input of ``ados-capabilities-codegen``, which turns the ``schemas/api/*.schema.json``
files into the generated TypeScript types the dashboard, the cockpit and
Mission Control import. The Rust-served bodies (``/api/status/full``, the cloud
heartbeat) are written by their crates' schema tests instead. The parity guard
``tests/test_api_schema_parity.py`` runs ``--check`` so a model change that
forgets to regenerate the file fails the suite. Do not edit the JSON by hand.

The schema describes the response as served: the route returns the model
through FastAPI's ``response_model``, which serializes every field, so every
property of every object is listed as required (a field with a default is
still always present on the wire).
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any

# Pin the base-directory resolution BEFORE any ados import so path-derived
# model defaults come out Linux-FHS-flavoured and machine-independent.
os.environ["ADOS_RUN_DIR"] = "/run/ados"
os.environ["ADOS_ETC_DIR"] = "/etc/ados"
os.environ["ADOS_VAR_DIR"] = "/var/ados"
for _var in ("ADOS_HOME", "ADOS_PAIRING_JSON", "ADOS_INSTALL_RESULT"):
    os.environ.pop(_var, None)

# Allow `python scripts/emit_api_schemas.py` from the repo root without an
# editable install.
ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from ados.setup.models import SetupStatus  # noqa: E402

SCHEMA_DIR = ROOT / "schemas" / "api"
SETUP_STATUS_PATH = SCHEMA_DIR / "setup-status.schema.json"


def _require_all(node: Any) -> None:
    """List every property of every object schema under ``node`` as required."""
    if isinstance(node, dict):
        props = node.get("properties")
        if isinstance(props, dict):
            node["required"] = sorted(props)
        for value in node.values():
            _require_all(value)
    elif isinstance(node, list):
        for value in node:
            _require_all(value)


def setup_status_schema() -> dict[str, Any]:
    """The serialized-response schema of ``SetupStatus``."""
    schema: dict[str, Any] = SetupStatus.model_json_schema(mode="serialization")
    _require_all(schema)
    return schema


def render(schema: dict[str, Any]) -> str:
    """The exact file body: deterministically ordered with a trailing newline."""
    return json.dumps(schema, indent=2, sort_keys=True) + "\n"


def targets() -> list[tuple[Path, str]]:
    return [(SETUP_STATUS_PATH, render(setup_status_schema()))]


def main(argv: list[str]) -> int:
    check = "--check" in argv[1:]
    drift = False
    for path, content in targets():
        if check:
            current = path.read_text(encoding="utf-8") if path.exists() else ""
            if current != content:
                print(
                    f"DRIFT: {path} is out of date with its model.\n"
                    "Run: python scripts/emit_api_schemas.py",
                    file=sys.stderr,
                )
                drift = True
            else:
                print(f"ok: {path}")
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="\n")
        print(f"wrote: {path}")
    return 1 if drift else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
