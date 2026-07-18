"""Emit the plugin-manifest JSON Schema from the Pydantic model.

Run as:

    python scripts/emit_manifest_schema.py            # (re)write the schema file
    python scripts/emit_manifest_schema.py --check    # verify, non-zero on drift

The committed schema at ``schemas/plugin-manifest.schema.json`` is generated from
``PluginManifest.model_json_schema()`` (via ``schema_dict()``) — it is the copy
the SDK type generator and the public docs consume, so it must stay in step with
the model. The ``--check`` mode mirrors the capability codegen's ``--check`` and
is run by the CI parity guard (``tests/plugins/test_manifest_schema_parity.py``)
so a model change that forgets to regenerate the file fails the suite. Do not
edit the JSON file by hand; edit the model and re-run this script.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

# Allow `python scripts/emit_manifest_schema.py` from the repo root without an
# editable install.
ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from ados.plugins.manifest import schema_dict  # noqa: E402

# The committed schema file the SDK + docs consume.
SCHEMA_PATH = ROOT / "schemas" / "plugin-manifest.schema.json"


def render() -> str:
    """The exact file body: the model's JSON Schema, deterministically ordered
    with a trailing newline (matching the ``emit_openapi`` convention)."""
    return json.dumps(schema_dict(), indent=2, sort_keys=True) + "\n"


def main(argv: list[str]) -> int:
    check = "--check" in argv[1:]
    content = render()
    if check:
        current = SCHEMA_PATH.read_text(encoding="utf-8") if SCHEMA_PATH.exists() else ""
        if current != content:
            print(
                f"DRIFT: {SCHEMA_PATH} is out of date with the PluginManifest model.\n"
                "Run: python scripts/emit_manifest_schema.py",
                file=sys.stderr,
            )
            return 1
        print(f"ok: {SCHEMA_PATH} matches the model")
        return 0
    SCHEMA_PATH.parent.mkdir(parents=True, exist_ok=True)
    SCHEMA_PATH.write_text(content, encoding="utf-8")
    print(f"plugin-manifest schema written to {SCHEMA_PATH}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
