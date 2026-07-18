"""CI parity guard: the committed plugin-manifest JSON Schema must match the model.

The schema at ``schemas/plugin-manifest.schema.json`` is generated from the
``PluginManifest`` Pydantic model by ``scripts/emit_manifest_schema.py``. This
guard fails if the committed file drifts from the model — the same discipline the
capability codegen ``--check`` enforces — so a model change that forgets to
regenerate the file is caught in CI rather than shipping a stale schema.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path

_ROOT = Path(__file__).resolve().parents[2]
_EMITTER = _ROOT / "scripts" / "emit_manifest_schema.py"


def _load_emitter():
    spec = importlib.util.spec_from_file_location("emit_manifest_schema", _EMITTER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_committed_schema_matches_the_model() -> None:
    emitter = _load_emitter()
    expected = emitter.render()
    committed = emitter.SCHEMA_PATH.read_text(encoding="utf-8")
    assert committed == expected, (
        "schemas/plugin-manifest.schema.json is out of date with the "
        "PluginManifest model; run: python scripts/emit_manifest_schema.py"
    )


def test_emitter_check_mode_passes() -> None:
    emitter = _load_emitter()
    assert emitter.main(["emit_manifest_schema.py", "--check"]) == 0
