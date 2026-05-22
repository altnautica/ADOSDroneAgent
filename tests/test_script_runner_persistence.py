"""Round-trip tests for the persistent script library.

The runner now persists scripts to ``SCRIPTS_DIR`` so the GCS Scripts
tab and the agent webapp share a single source of truth. The
in-memory execution state is unrelated to the save / list / delete
flow these tests exercise.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock

import pytest

from ados.services.scripting import script_runner as runner_mod
from ados.services.scripting.script_runner import ScriptRunner


@pytest.fixture
def runner(tmp_path: Path, monkeypatch) -> ScriptRunner:
    scripts_dir = tmp_path / "scripts"
    monkeypatch.setattr(runner_mod, "SCRIPTS_DIR", scripts_dir)

    config = MagicMock()
    config.max_concurrent = 4
    executor = MagicMock()
    return ScriptRunner(config=config, executor=executor, sdk_port=8892)


def test_save_script_returns_record_with_server_assigned_id(runner: ScriptRunner) -> None:
    saved = runner.save_script("First", "print('hi')", "telemetry")
    assert saved.name == "First"
    assert saved.content == "print('hi')"
    assert saved.suite == "telemetry"
    assert len(saved.id) == 12
    assert saved.lastModified  # ISO timestamp populated


def test_save_then_list_round_trips(runner: ScriptRunner) -> None:
    runner.save_script("Alpha", "a = 1\n", None)
    runner.save_script("Beta", "b = 2\n", "navigation")
    listed = sorted(runner.list_saved_scripts(), key=lambda s: s.name)
    assert [s.name for s in listed] == ["Alpha", "Beta"]
    assert listed[1].suite == "navigation"


def test_save_with_same_name_replaces_in_place(runner: ScriptRunner) -> None:
    first = runner.save_script("Same", "v = 1\n")
    second = runner.save_script("Same", "v = 2\n")
    assert first.id == second.id
    assert second.content == "v = 2\n"
    assert len(runner.list_saved_scripts()) == 1


def test_save_rejects_blank_name(runner: ScriptRunner) -> None:
    with pytest.raises(RuntimeError, match="name required"):
        runner.save_script("", "noop")
    with pytest.raises(RuntimeError, match="name required"):
        runner.save_script("   ", "noop")


def test_delete_script_clears_disk(runner: ScriptRunner) -> None:
    saved = runner.save_script("Doomed", "print(1)")
    assert runner.delete_script(saved.id) is True
    assert runner.get_saved_script(saved.id) is None
    assert runner.list_saved_scripts() == []


def test_delete_unknown_id_returns_false(runner: ScriptRunner) -> None:
    # Well-formed but absent.
    assert runner.delete_script("0" * 12) is False


def test_delete_rejects_malformed_id(runner: ScriptRunner) -> None:
    # Path traversal attempt + non-hex characters; the id regex blocks
    # both before any filesystem call.
    assert runner.delete_script("../etc/passwd") is False
    assert runner.delete_script("ZZZZZZZZZZZZ") is False
    assert runner.delete_script("short") is False


def test_get_saved_script_rejects_malformed_id(runner: ScriptRunner) -> None:
    assert runner.get_saved_script("../etc/passwd") is None


def test_saved_file_is_restricted_to_owner(runner: ScriptRunner) -> None:
    saved = runner.save_script("Perms", "print(1)")
    path = runner._saved_path(saved.id)
    mode = path.stat().st_mode & 0o777
    # 0o600: only the agent user can read or write. The persisted file
    # can contain operator-authored scripts that may carry sensitive
    # snippets; group + other have no business reading it.
    assert mode == 0o600


def test_save_rejects_oversized_content(runner: ScriptRunner) -> None:
    """A 256 KiB ceiling caps the per-script wire payload."""
    payload = "x" * (256 * 1024 + 1)
    with pytest.raises(RuntimeError, match="exceeds"):
        runner.save_script("Big", payload)


def test_save_rejects_when_library_full(
    runner: ScriptRunner, monkeypatch
) -> None:
    """The hard ceiling on library size kicks in only for net-new
    saves; in-place updates of an existing record continue to work."""
    from ados.services.scripting import script_runner as runner_mod

    monkeypatch.setattr(runner_mod, "_MAX_SAVED_SCRIPTS", 3)

    runner.save_script("a", "print(1)")
    runner.save_script("b", "print(2)")
    runner.save_script("c", "print(3)")
    # Net-new save past the cap is rejected.
    with pytest.raises(RuntimeError, match="library full"):
        runner.save_script("d", "print(4)")
    # In-place update of an existing record at the cap still works.
    updated = runner.save_script("a", "print('a2')")
    assert updated.content == "print('a2')"
