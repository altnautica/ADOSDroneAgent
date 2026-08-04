"""The runner must create the plugin's per-drone data dir before it hands the
plugin a context, and must honor ``ADOS_PLUGIN_DATA_DIR`` when the host set it.

Nothing upstream creates the per-drone leaf under ``/var/ados/plugin-data`` —
the installer makes only the base — so a plugin opening ``ctx.data_dir / x`` on
its first write would hit ``FileNotFoundError``. These tests exercise the pure
``_prepare_plugin_dirs`` helper the runner calls, and assert a file can actually
be written under the returned dir (not merely that a path string was computed).
"""

from __future__ import annotations

import ados.plugins.runner as runner


def test_prepare_plugin_dirs_creates_a_writable_data_dir(tmp_path, monkeypatch):
    monkeypatch.setattr(runner, "PLUGIN_DATA_DIR", tmp_path / "plugin-data")
    monkeypatch.setattr(runner, "PLUGIN_RUN_DIR", tmp_path / "run")
    monkeypatch.delenv("ADOS_PLUGIN_DATA_DIR", raising=False)

    data_dir, config_dir, temp_dir = runner._prepare_plugin_dirs(
        "com.example.plugin", "drone-xyz"
    )

    # The per-drone leaf must exist and be writable, not just resolved.
    assert data_dir.is_dir()
    (data_dir / "state.json").write_text("{}")
    assert config_dir.is_dir()
    assert temp_dir.is_dir()
    # Fallback derivation, since no env was set.
    assert data_dir == tmp_path / "plugin-data" / "com.example.plugin" / "drones" / "drone-xyz"


def test_env_data_dir_wins_over_the_local_derivation(tmp_path, monkeypatch):
    monkeypatch.setattr(runner, "PLUGIN_DATA_DIR", tmp_path / "plugin-data")
    monkeypatch.setattr(runner, "PLUGIN_RUN_DIR", tmp_path / "run")
    host_dir = tmp_path / "host-supplied" / "drones" / "drone-xyz"
    monkeypatch.setenv("ADOS_PLUGIN_DATA_DIR", str(host_dir))

    data_dir, _config_dir, _temp_dir = runner._prepare_plugin_dirs(
        "com.example.plugin", "drone-xyz"
    )

    assert data_dir == host_dir
    assert data_dir.is_dir()
    (data_dir / "state.json").write_text("{}")


def test_idempotent_across_repeated_calls(tmp_path, monkeypatch):
    monkeypatch.setattr(runner, "PLUGIN_DATA_DIR", tmp_path / "plugin-data")
    monkeypatch.setattr(runner, "PLUGIN_RUN_DIR", tmp_path / "run")
    monkeypatch.delenv("ADOS_PLUGIN_DATA_DIR", raising=False)

    first = runner._prepare_plugin_dirs("com.example.plugin", "drone-xyz")
    (first[0] / "state.json").write_text("{}")
    second = runner._prepare_plugin_dirs("com.example.plugin", "drone-xyz")

    assert first == second
    # The write survived the second call (exist_ok, not a wipe).
    assert (second[0] / "state.json").read_text() == "{}"
