"""CI parity guard: the committed REST response schemas must match the models.

``schemas/api/setup-status.schema.json`` is generated from the ``SetupStatus``
Pydantic model by ``scripts/emit_api_schemas.py`` and feeds the TypeScript
codegen (``ados-capabilities-codegen``) that the dashboard, the cockpit and
Mission Control import their setup-status types from. This guard fails when
the committed file drifts from the model, so a model change that forgets to
regenerate the schema is caught here rather than shipping stale client types.

The emitter runs in a SUBPROCESS: it pins the ``ADOS_*_DIR`` base-path env
vars before importing ``ados``, which an in-process import here could not
guarantee.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

_ROOT = Path(__file__).resolve().parents[1]
_EMITTER = _ROOT / "scripts" / "emit_api_schemas.py"
_SETUP_STATUS = _ROOT / "schemas" / "api" / "setup-status.schema.json"


def test_committed_schemas_match_the_models() -> None:
    proc = subprocess.run(
        [sys.executable, str(_EMITTER), "--check"],
        capture_output=True,
        text=True,
        cwd=_ROOT,
        timeout=120,
    )
    assert proc.returncode == 0, (
        "schemas/api/*.schema.json is out of date with the response models; "
        f"run: python scripts/emit_api_schemas.py\n{proc.stderr}"
    )


def test_setup_status_schema_lists_every_served_field_as_required() -> None:
    """The route serializes every field, so the schema requires all of them.

    A field with a model default (``lan_host``, ``paired``) is still always on
    the wire, and the generated client type must not mark it optional.
    """
    schema = json.loads(_SETUP_STATUS.read_text(encoding="utf-8"))
    assert schema["title"] == "SetupStatus"
    assert sorted(schema["required"]) == sorted(schema["properties"])
    for name in ("lan_host", "paired", "hardware_check", "profile_suggestion"):
        assert name in schema["required"], f"{name} is served on every response"
    for name, node in schema["$defs"].items():
        if "properties" in node:
            assert sorted(node["required"]) == sorted(node["properties"]), name
