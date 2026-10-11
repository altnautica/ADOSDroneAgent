"""Tests for scripts/emit-reference.py (the docs reference exporter)."""

from __future__ import annotations

import importlib.util
from pathlib import Path

import click
import pytest

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "emit-reference.py"


@pytest.fixture(scope="module")
def er():
    spec = importlib.util.spec_from_file_location("emit_reference", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _fixture_group() -> click.Group:
    @click.group()
    def root() -> None:
        """Root."""

    @root.group()
    def plugin() -> None:
        """Manage plugins."""

    @plugin.command()
    @click.argument("plugin_id")
    @click.option("--lines", "-n", type=int, default=50, help="Lines to show.")
    @click.option("--follow/--no-follow", default=False, help="Stream.")
    def logs(plugin_id: str, lines: int, follow: bool) -> None:
        """Print a plugin's logs."""

    @root.command(hidden=True)
    def secret() -> None:
        """Hidden."""

    @root.command()
    @click.option("--token", hidden=True)
    def version(token: str) -> None:
        """Print the version."""

    return root


def test_cli_walk_is_deterministic_and_skips_hidden(er):
    first = er.cli_commands(_fixture_group())
    assert first == er.cli_commands(_fixture_group())
    paths = [c["path"] for c in first]
    assert paths == ["plugin", "plugin logs", "version"]
    logs = first[1]
    assert logs["help"] == "Print a plugin's logs."
    assert logs["arguments"] == [{"name": "plugin_id", "required": True}]
    assert logs["options"][0] == {
        "flags": ["--lines", "-n"],
        "type": "integer",
        "default": 50,
        "required": False,
        "help": "Lines to show.",
    }
    assert logs["options"][1]["flags"] == ["--follow", "--no-follow"]
    assert first[2]["options"] == []
    text = er.dumps({"schema": 1, "commands": first})
    assert text.endswith("}\n")
    assert text == er.dumps({"commands": first, "schema": 1})


def test_cli_walk_lists_advanced_groups_but_not_their_hidden_commands(er):
    root = _fixture_group()

    @click.group(hidden=True)
    def config() -> None:
        """Inspect the on-disk config."""

    @config.command()
    def show() -> None:
        """Print the config."""

    @config.command(hidden=True)
    def internal() -> None:
        """Internal."""

    root.add_command(config)
    assert "config" not in [c["path"] for c in er.cli_commands(root)]
    paths = [c["path"] for c in er.cli_commands(root, (config,))]
    assert paths == ["config", "config show", "plugin", "plugin logs", "version"]


def test_the_agent_cli_reference_lists_every_advanced_group(er):
    from ados.cli.main import ADVANCED_GROUPS

    paths = {c["path"] for c in er.emit_cli()["commands"]}
    for group in ADVANCED_GROUPS:
        assert group.name in paths, group.name
    assert "config" in paths
    # The installer's entry stays internal.
    assert "install" not in paths



def test_unset_option_defaults_match_across_click_versions(er):
    @click.command()
    @click.option("--name", help="No default.")
    @click.option("--force", is_flag=True, help="Boolean flag, no default.")
    def cmd(name: str, force: bool) -> None:
        """Command."""

    name, force = cmd.params
    # click 8.3+ leaves these as its UNSET sentinel, 8.1 as None / False: both
    # emit the 8.1 value, never the sentinel's repr.
    assert er._option_default(name) is None
    assert er._option_default(force) is False


def test_flatten_schema_resolves_refs_and_optionals(er):
    schema = {
        "$defs": {
            "Inner": {
                "properties": {
                    "port": {"type": "integer", "default": 8080, "minimum": 1},
                    "key": {"type": "string", "x-secret": True, "default": ""},
                },
                "type": "object",
            }
        },
        "properties": {
            "inner": {"$ref": "#/$defs/Inner"},
            "mode": {"enum": ["a", "b"], "type": "string", "default": "a"},
            "maybe": {"anyOf": [{"type": "integer"}, {"type": "null"}], "default": None},
        },
        "required": ["mode"],
        "type": "object",
    }
    rows = {r["path"]: r for r in er.flatten_schema(schema)}
    assert sorted(rows) == ["inner.key", "inner.port", "maybe", "mode"]
    assert rows["inner.port"]["default"] == 8080
    assert rows["inner.port"]["minimum"] == 1
    assert rows["inner.key"]["secret"] is True
    assert rows["mode"]["required"] is True
    assert rows["mode"]["enum"] == ["a", "b"]
    assert rows["maybe"]["type"] == "integer|null"


def test_check_exits_1_on_drift(er, tmp_path, monkeypatch, capsys):
    fake = {name: (lambda n=name: {"schema": 1, "name": n}) for name in er.FILES}
    monkeypatch.setattr(er, "EMITTERS", fake)
    assert er.main(["--out", str(tmp_path)]) == 0
    assert er.main(["--check", "--out", str(tmp_path)]) == 0

    cli = tmp_path / "cli.json"
    cli.write_text(cli.read_text("utf-8").replace('"cli"', '"cli-planted"'), "utf-8")
    assert er.main(["--check", "--out", str(tmp_path)]) == 1
    assert "cli.json" in capsys.readouterr().err
