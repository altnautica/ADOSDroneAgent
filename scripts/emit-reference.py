#!/usr/bin/env python3
"""Export the agent's public surfaces as JSON for the documentation site.

Writes nine files under ``docs/reference/`` (each ``{"schema": 1, ...}``),
read from the code that defines each surface rather than a hand-kept list:

* ``cli.json`` -- the ``ados`` click tree (hidden commands skipped);
* ``routes.json`` -- every HTTP route, from ``gen-api-surface.py``'s producers;
* ``config.json`` -- ``schemas/agent-config.schema.json`` flattened to keys;
* ``capabilities.json`` -- the capability catalog, via the codegen;
* ``manifest.json`` -- ``schemas/plugin-manifest.schema.json`` flattened;
* ``boards.json`` -- the HAL board profiles;
* ``units.json`` -- the shipped systemd units (``--example units``);
* ``installer-flags.json`` -- the installer flag table (``--example flags``);
* ``dashboard-routes.json`` -- ``dashboard/src/route-table.json``.

Usage::

    python scripts/emit-reference.py [--check] [--out docs/reference]

``--check`` regenerates into a temporary directory and exits 1, naming each
file that differs from the committed copy.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

import yaml

REPO = Path(__file__).resolve().parent.parent
CRATES = REPO / "crates"
DEFAULT_OUT = REPO / "docs" / "reference"

FILES = (
    "cli",
    "routes",
    "config",
    "capabilities",
    "manifest",
    "boards",
    "units",
    "installer-flags",
    "dashboard-routes",
)


def dumps(data: dict[str, Any]) -> str:
    return json.dumps(data, sort_keys=True, indent=2, ensure_ascii=False) + "\n"


def cargo_json(*args: str) -> Any:
    proc = subprocess.run(
        ["cargo", "run", "--quiet", *args],
        cwd=CRATES,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"cargo run {' '.join(args)} failed")
    return json.loads(proc.stdout)


def _json_value(value: Any) -> Any:
    """A click default as JSON: plain scalars and lists pass, others stringify."""
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return [_json_value(v) for v in value]
    return str(value)


def cli_commands(root: Any) -> list[dict[str, Any]]:
    """Every visible command under ``root`` (a click group), sorted by path.

    ``path`` is the space-joined command path without the program name.
    """
    import click

    out: list[dict[str, Any]] = []

    def walk(cmd: click.Command, path: list[str]) -> None:
        if path:
            arguments = []
            options = []
            for p in cmd.params:
                if isinstance(p, click.Argument):
                    arguments.append({"name": p.name, "required": bool(p.required)})
                elif isinstance(p, click.Option) and not p.hidden:
                    default = None if callable(p.default) else _json_value(p.default)
                    options.append(
                        {
                            "flags": list(p.opts) + list(p.secondary_opts),
                            "type": p.type.name,
                            "default": default,
                            "required": bool(p.required),
                            "help": p.help,
                        }
                    )
            out.append(
                {
                    "path": " ".join(path),
                    "help": (cmd.help or cmd.short_help or "").strip() or None,
                    "arguments": arguments,
                    "options": options,
                }
            )
        if isinstance(cmd, click.Group):
            for name, sub in cmd.commands.items():
                if not sub.hidden:
                    walk(sub, [*path, name])

    walk(root, [])
    return sorted(out, key=lambda c: c["path"])


def emit_cli() -> dict[str, Any]:
    from ados.cli.main import cli

    return {"schema": 1, "commands": cli_commands(cli)}


def _load_api_surface() -> Any:
    spec = importlib.util.spec_from_file_location(
        "gen_api_surface", REPO / "scripts" / "gen-api-surface.py"
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def emit_routes() -> dict[str, Any]:
    gas = _load_api_surface()
    native = sorted(set(gas.native_routes()), key=lambda r: (r[1], r[0]))
    residual = sorted(set(gas.residual_routes()), key=lambda r: (r[1], r[0]))
    residual = gas.shadow_native(native, residual)

    def row(method: str, path: str, served_by: str, note: str) -> dict[str, Any]:
        return {
            "method": method,
            "path": path,
            "served_by": served_by,
            "public": (method, path) in gas.UNAUTHENTICATED,
            "websocket": method == "WS",
            "relay_forbidden": path in gas.RELAY_FORBIDDEN,
            "notes": note or None,
        }

    rows = [row(m, p, "native", gas.note_for(m, p)) for m, p in native]
    rows += [row(m, p, "residual", gas.note_for(m, p)) for m, p in residual]
    rows += [row(m, p, "front", note) for m, p, note in gas.FRONT_FALLBACK]
    rows += [
        row("GET", p, "logd", f"ados-logd on :{gas.LOGD_PORT}") for p in gas.LOGD_PATHS
    ]
    rows.sort(key=lambda r: (r["path"], r["method"], r["served_by"]))
    return {"schema": 1, "routes": rows}


def _resolve(schema: dict[str, Any], node: dict[str, Any]) -> dict[str, Any]:
    """Follow ``$ref`` and unwrap ``Optional`` (``anyOf`` with ``null``)."""
    while True:
        if "$ref" in node:
            target = schema["$defs"][node["$ref"].rsplit("/", 1)[-1]]
            node = {**target, **{k: v for k, v in node.items() if k != "$ref"}}
            continue
        variants = node.get("anyOf")
        if variants:
            non_null = [v for v in variants if v.get("type") != "null"]
            if len(non_null) == 1:
                rest = {k: v for k, v in node.items() if k != "anyOf"}
                node = {**_resolve(schema, non_null[0]), **rest}
                if len(non_null) != len(variants):
                    node["nullable"] = True
                continue
        return node


def _type_of(node: dict[str, Any]) -> str | None:
    if "type" in node:
        t = node["type"]
        base = "|".join(t) if isinstance(t, list) else t
    elif "anyOf" in node:
        base = "|".join(str(v.get("type", "object")) for v in node["anyOf"])
    elif "enum" in node:
        base = "enum"
    else:
        return None
    return f"{base}|null" if node.get("nullable") else base


def flatten_schema(schema: dict[str, Any]) -> list[dict[str, Any]]:
    """One row per leaf field, dotted path; array-of-object items as ``path[]``."""
    rows: list[dict[str, Any]] = []

    def walk(node: dict[str, Any], prefix: str, required: bool, default: Any) -> None:
        node = _resolve(schema, node)
        props = node.get("properties")
        if props:
            req = set(node.get("required", []))
            own_default = node.get("default", default)
            for name, sub in props.items():
                sub_default = own_default.get(name) if isinstance(own_default, dict) else None
                if "default" in _resolve(schema, sub):
                    sub_default = _resolve(schema, sub)["default"]
                walk(sub, f"{prefix}.{name}" if prefix else name, name in req, sub_default)
            return
        items = node.get("items")
        if node.get("type") == "array" and isinstance(items, dict):
            item = _resolve(schema, items)
            if item.get("properties"):
                rows.append(_leaf(node, prefix, required, default))
                walk(item, f"{prefix}[]", False, None)
                return
        rows.append(_leaf(node, prefix, required, default))

    walk(schema, "", True, None)
    return sorted(rows, key=lambda r: r["path"])


def _leaf(node: dict[str, Any], path: str, required: bool, default: Any) -> dict[str, Any]:
    return {
        "path": path,
        "type": _type_of(node),
        "required": required,
        "default": node.get("default", default),
        "enum": node.get("enum"),
        "minimum": node.get("minimum", node.get("exclusiveMinimum")),
        "maximum": node.get("maximum", node.get("exclusiveMaximum")),
        "description": node.get("description") or node.get("title"),
        "secret": bool(node.get("x-secret", False)),
    }


def emit_config() -> dict[str, Any]:
    schema = json.loads((REPO / "schemas" / "agent-config.schema.json").read_text("utf-8"))
    keys = [
        {
            "key": r["path"],
            "type": r["type"],
            "default": r["default"],
            "enum": r["enum"],
            "minimum": r["minimum"],
            "maximum": r["maximum"],
            "description": r["description"],
            "secret": r["secret"],
        }
        for r in flatten_schema(schema)
    ]
    return {"schema": 1, "keys": keys}


def emit_manifest() -> dict[str, Any]:
    schema = json.loads(
        (REPO / "schemas" / "plugin-manifest.schema.json").read_text("utf-8")
    )
    fields = [
        {
            "path": r["path"],
            "type": r["type"],
            "required": r["required"],
            "default": r["default"],
            "enum": r["enum"],
            "description": r["description"],
        }
        for r in flatten_schema(schema)
    ]
    return {"schema": 1, "fields": fields}


def emit_capabilities() -> dict[str, Any]:
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "capabilities.json"
        proc = subprocess.run(
            [
                "cargo", "run", "--quiet", "-p", "ados-capabilities-codegen", "--",
                "--emit-capabilities-json", str(path),
            ],
            cwd=CRATES,
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr)
            raise SystemExit("emitting the capability catalog failed")
        return json.loads(path.read_text("utf-8"))


def emit_boards() -> dict[str, Any]:
    boards = []
    for f in sorted((REPO / "src" / "ados" / "hal" / "boards").glob("*.yaml")):
        b = yaml.safe_load(f.read_text("utf-8")) or {}
        video = b.get("video") or {}
        nav = b.get("navigation") or {}
        soc = b.get("soc")
        tokens = [f.stem]
        if soc and soc.lower() not in tokens:
            tokens.append(soc.lower())
        boards.append(
            {
                "id": f.stem,
                "name": b.get("name"),
                "vendor": b.get("vendor"),
                "soc": soc,
                "arch": b.get("arch"),
                "default_tier": b.get("default_tier"),
                "hw_video_codecs": b.get("hw_video_codecs") or [],
                "max_encode_resolution": video.get("max_encode_resolution"),
                "vio_capable": nav.get("vio_capable"),
                "plugin_tokens": tokens,
            }
        )
    return {"schema": 1, "boards": boards}


def emit_dashboard_routes() -> dict[str, Any]:
    table = json.loads((REPO / "dashboard" / "src" / "route-table.json").read_text("utf-8"))
    return {"schema": 1, "routes": table}


EMITTERS = {
    "cli": emit_cli,
    "routes": emit_routes,
    "config": emit_config,
    "capabilities": emit_capabilities,
    "manifest": emit_manifest,
    "boards": emit_boards,
    "units": lambda: cargo_json("-p", "ados-control", "--example", "units"),
    "installer-flags": lambda: cargo_json("-p", "ados-installer", "--example", "flags"),
    "dashboard-routes": emit_dashboard_routes,
}


def write_all(out: Path) -> None:
    out.mkdir(parents=True, exist_ok=True)
    for name in FILES:
        data = EMITTERS[name]()
        if data.get("schema") != 1:
            raise SystemExit(f"{name}: exporter did not produce schema 1")
        (out / f"{name}.json").write_text(dumps(data), encoding="utf-8", newline="\n")


def check(out: Path) -> list[str]:
    """Names of the files under ``out`` that differ from a fresh export."""
    with tempfile.TemporaryDirectory() as tmp:
        fresh = Path(tmp)
        write_all(fresh)
        drift = []
        for name in FILES:
            committed = out / f"{name}.json"
            current = committed.read_text("utf-8") if committed.exists() else None
            if current != (fresh / f"{name}.json").read_text("utf-8"):
                drift.append(f"{name}.json")
        return drift


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    args = parser.parse_args(argv)
    out = args.out if args.out.is_absolute() else Path.cwd() / args.out
    if args.check:
        drift = check(out)
        for name in drift:
            print(f"DRIFT: {out / name}", file=sys.stderr)
        if drift:
            print("Run: python scripts/emit-reference.py", file=sys.stderr)
            return 1
        print(f"ok: {len(FILES)} reference files current")
        return 0
    write_all(out)
    print(f"wrote {len(FILES)} files to {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
