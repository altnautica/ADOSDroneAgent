"""Round-trip tests for the new one-shot nudge state.

The dashboard surfaces a one-shot toast on Home for the cloud-posture
default flip. The agent owns the suppression flag so the prompt
follows the agent through browser cache wipes.
"""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

from ados.setup import state as setup_state


def test_ack_nudge_persists_to_disk(tmp_path: Path, monkeypatch) -> None:
    state_dir = tmp_path / "setup"
    state_path = state_dir / "state.json"
    monkeypatch.setattr(setup_state, "SETUP_STATE_DIR", state_dir)
    monkeypatch.setattr(setup_state, "SETUP_STATE_PATH", state_path)

    # Cold read returns the empty default.
    initial = setup_state.read_state()
    assert initial.acked_nudges == set()

    # Acking lands the id and persists.
    setup_state.ack_nudge("cloud_posture_default_changed")
    raw = json.loads(state_path.read_text(encoding="utf-8"))
    assert raw["acked_nudges"] == ["cloud_posture_default_changed"]

    # Re-read deserialises the set verbatim.
    reread = setup_state.read_state()
    assert reread.acked_nudges == {"cloud_posture_default_changed"}


def test_ack_nudge_is_idempotent(tmp_path: Path, monkeypatch) -> None:
    state_dir = tmp_path / "setup"
    state_path = state_dir / "state.json"
    monkeypatch.setattr(setup_state, "SETUP_STATE_DIR", state_dir)
    monkeypatch.setattr(setup_state, "SETUP_STATE_PATH", state_path)

    setup_state.ack_nudge("cloud_posture_default_changed")
    setup_state.ack_nudge("cloud_posture_default_changed")
    state = setup_state.read_state()
    assert state.acked_nudges == {"cloud_posture_default_changed"}


def test_ack_nudge_survives_other_state_mutations(
    tmp_path: Path, monkeypatch
) -> None:
    """Acking a nudge, then running unrelated state mutations, must
    not lose the acked entry. The persisted shape carries every
    field side-by-side."""
    state_dir = tmp_path / "setup"
    state_path = state_dir / "state.json"
    monkeypatch.setattr(setup_state, "SETUP_STATE_DIR", state_dir)
    monkeypatch.setattr(setup_state, "SETUP_STATE_PATH", state_path)

    setup_state.ack_nudge("cloud_posture_default_changed")
    setup_state.mark_setup_skipped()
    setup_state.mark_skipped("pair")

    final = setup_state.read_state()
    assert final.acked_nudges == {"cloud_posture_default_changed"}
    assert final.setup_skipped is True
    assert "pair" in final.skipped_steps


def test_read_state_drops_corrupted_acked_nudges_field(
    tmp_path: Path, monkeypatch
) -> None:
    """A non-list acked_nudges field from an externally-edited state
    file must not crash the read path."""
    state_dir = tmp_path / "setup"
    state_path = state_dir / "state.json"
    state_dir.mkdir(parents=True)
    state_path.write_text(
        json.dumps({"acked_nudges": "not_a_list"}),
        encoding="utf-8",
    )
    monkeypatch.setattr(setup_state, "SETUP_STATE_DIR", state_dir)
    monkeypatch.setattr(setup_state, "SETUP_STATE_PATH", state_path)

    state = setup_state.read_state()
    assert state.acked_nudges == set()


def test_ack_nudge_swallows_write_failure(
    tmp_path: Path, monkeypatch
) -> None:
    """A read-only filesystem must not propagate up — the in-memory
    state still reflects the ack so the API can flip its UI even on
    a dev box where SETUP_STATE_DIR is not writable."""
    state_dir = tmp_path / "setup"
    state_path = state_dir / "state.json"
    monkeypatch.setattr(setup_state, "SETUP_STATE_DIR", state_dir)
    monkeypatch.setattr(setup_state, "SETUP_STATE_PATH", state_path)

    def _boom(*_args, **_kwargs) -> None:
        raise OSError("read-only")

    with patch.object(setup_state, "_write", _boom):
        state = setup_state.ack_nudge("cloud_posture_default_changed")

    assert state.acked_nudges == {"cloud_posture_default_changed"}
