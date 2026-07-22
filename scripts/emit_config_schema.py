"""Emit the agent-config JSON Schema from the Pydantic model.

Run as:

    python scripts/emit_config_schema.py            # (re)write the schema file
    python scripts/emit_config_schema.py --check    # verify, non-zero on drift

The committed schema at ``schemas/agent-config.schema.json`` is generated from
``ADOSConfig.model_json_schema()`` and served natively by the control surface
at ``GET /api/config/schema`` (embedded at build time), so a schema-driven
settings UI can render and validate the config without hand-typed forms. It
must stay in step with the model: the ``--check`` mode mirrors
``scripts/emit_manifest_schema.py`` and is run by the CI parity guard
(``tests/test_config_schema_parity.py``) so a model change that forgets to
regenerate the file fails the suite. Do not edit the JSON file by hand; edit
the model and re-run this script.

Two augmentations over the raw model emit:

* **Deterministic, target-flavoured defaults.** Several model defaults are
  filesystem paths resolved through ``ados.core.paths`` at import time, which
  honours the ``ADOS_RUN_DIR``/``ADOS_ETC_DIR``/``ADOS_VAR_DIR`` overrides and
  the macOS per-user layout. The env pin below (before any ``ados`` import)
  locks those to the Linux FHS bases the agent actually runs with, so the
  emitted file is byte-identical no matter which machine regenerates it. This
  is also why the parity guard runs this script in a SUBPROCESS: an in-process
  import could see ``ados.core.paths`` already loaded with host-flavoured
  constants.

* **Secret markers.** Every field the config values-read redacts, plus every
  plaintext credential the model carries, gets ``"x-secret": true`` on its
  property node so a schema-driven UI renders set/not-set instead of the
  value. The marker set must stay in step with the values-read redaction in
  ``src/ados/api/routes/config.py``; a path that stops resolving on the model
  fails the emit loudly rather than silently dropping a marker.

Known divergence: the values-read (``GET /api/config``) injects
``agent.board_override`` from a file on disk; it is not a model field, so the
schema deliberately does not describe it.
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

# Allow `python scripts/emit_config_schema.py` from the repo root without an
# editable install.
ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from ados.core.config import ADOSConfig  # noqa: E402

# The committed schema file the native control surface embeds and serves.
SCHEMA_PATH = ROOT / "schemas" / "agent-config.schema.json"

# Dotted config paths whose VALUES must never render in a UI: the four paths
# the values-read redacts today plus the plaintext credentials the model
# carries. Each gets `x-secret: true` on its property node.
SECRET_PATHS: tuple[str, ...] = (
    "security.tls.key_path",
    "security.api.api_key",
    "security.wireguard.config_path",
    "server.self_hosted.api_key",
    "security.hmac_secret",
    "server.mqtt_password",
    "network.wifi_client.password",
    "network.hotspot.password",
)


def _deref(schema: dict[str, Any], node: dict[str, Any]) -> dict[str, Any]:
    """Resolve a property node to the object schema holding its properties.

    Follows ``$ref`` (with or without sibling keys) and the single-element
    ``allOf`` wrapper older pydantic emits, into ``$defs``.
    """
    seen = 0
    while seen < 10:
        seen += 1
        if "$ref" in node:
            ref = node["$ref"]
            if not ref.startswith("#/$defs/"):
                raise SystemExit(f"unsupported $ref target: {ref}")
            node = schema["$defs"][ref.rsplit("/", 1)[-1]]
            continue
        if "allOf" in node and len(node["allOf"]) == 1 and "$ref" in node["allOf"][0]:
            node = node["allOf"][0]
            continue
        return node
    raise SystemExit("$ref chain too deep — cyclic schema?")


def _mark_secret(schema: dict[str, Any], dotted: str) -> None:
    """Set ``x-secret: true`` on the property node ``dotted`` resolves to.

    Fails loudly when the path does not exist on the model, so a field rename
    breaks the emit (and the CI parity guard) instead of silently shipping a
    schema with a dropped secret marker.
    """
    node: dict[str, Any] = schema
    parts = dotted.split(".")
    for i, part in enumerate(parts):
        node = _deref(schema, node)
        props = node.get("properties", {})
        if part not in props:
            raise SystemExit(
                f"secret path {dotted!r} does not resolve on the model "
                f"(no property {part!r})"
            )
        if i == len(parts) - 1:
            props[part]["x-secret"] = True
        else:
            node = props[part]


def schema_dict() -> dict[str, Any]:
    """The model's JSON Schema with the secret markers applied."""
    schema = ADOSConfig.model_json_schema()
    for dotted in SECRET_PATHS:
        _mark_secret(schema, dotted)
    return schema


def render() -> str:
    """The exact file body: deterministically ordered with a trailing newline
    (matching the ``emit_manifest_schema`` convention)."""
    return json.dumps(schema_dict(), indent=2, sort_keys=True) + "\n"


def main(argv: list[str]) -> int:
    check = "--check" in argv[1:]
    content = render()
    if check:
        current = SCHEMA_PATH.read_text(encoding="utf-8") if SCHEMA_PATH.exists() else ""
        if current != content:
            print(
                f"DRIFT: {SCHEMA_PATH} is out of date with the ADOSConfig model.\n"
                "Run: python scripts/emit_config_schema.py",
                file=sys.stderr,
            )
            return 1
        print(f"ok: {SCHEMA_PATH} matches the model")
        return 0
    SCHEMA_PATH.parent.mkdir(parents=True, exist_ok=True)
    SCHEMA_PATH.write_text(content, encoding="utf-8")
    print(f"agent-config schema written to {SCHEMA_PATH}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
