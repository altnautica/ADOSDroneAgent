# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""The operator audit trail: the writer, and the path it shares with its readers.

`/var/ados/audit.jsonl` was budgeted by the supervisor's disk janitor, trimmed on
a rung, counted toward the agent's footprint and rendered by `ados diag` as "audit
trail" — with no writer anywhere. A category an operator can see and nothing
produces implies a trail exists; these tests exist so it does.

The readers constrain exactly two properties (append-only, newline-delimited,
because `reclaim::trim_append_only` cuts on a record boundary) and one more that
no test could infer: that the writer and the trimmer name the SAME FILE. The
cross-language check at the bottom is the one that would have caught a rename.
"""

from __future__ import annotations

import importlib
import json
import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]


@pytest.fixture
def audit(tmp_path, monkeypatch):
    """`ados.core.audit` writing into a tempdir."""
    import ados.core.audit as mod
    import ados.core.paths as paths

    monkeypatch.setattr(paths, "AUDIT_LOG", tmp_path / "audit.jsonl")
    monkeypatch.setattr(mod, "AUDIT_LOG", tmp_path / "audit.jsonl")
    return mod


def _records(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def test_a_record_is_one_newline_terminated_json_object(audit, tmp_path) -> None:
    audit.record(
        audit.REGULATORY_POSTURE_APPLIED,
        audit.ACTOR_OPERATOR,
        {"mode": "region", "region": "IN"},
    )
    raw = (tmp_path / "audit.jsonl").read_text()
    assert raw.endswith("\n"), (
        "the janitor trims on a newline boundary; a record without one takes the "
        "next record with it when the file is cut"
    )
    assert raw.count("\n") == 1
    rec = json.loads(raw)
    assert set(rec) == {"ts", "kind", "actor", "detail"}
    assert rec["kind"] == "regulatory.posture_applied"
    assert rec["actor"] == "operator"
    assert rec["detail"] == {"mode": "region", "region": "IN"}
    assert isinstance(rec["ts"], int) and rec["ts"] > 1_700_000_000_000


def test_records_append_rather_than_replace(audit, tmp_path) -> None:
    for i in range(3):
        audit.record(audit.PLUGIN_SANDBOX_VIOLATION, audit.ACTOR_SERVICE, {"n": i})
    recs = _records(tmp_path / "audit.jsonl")
    assert [r["detail"]["n"] for r in recs] == [0, 1, 2], (
        "the trail is append-only; a truncating open loses every earlier decision"
    )


def test_an_unwritable_path_is_logged_and_never_raised(audit, tmp_path, monkeypatch) -> None:
    # An operator must not be refused a regulatory change because a disk is full.
    blocked = tmp_path / "not-a-dir" / "audit.jsonl"
    monkeypatch.setattr(audit, "AUDIT_LOG", blocked)
    monkeypatch.setattr(
        Path, "mkdir", lambda *a, **k: (_ for _ in ()).throw(OSError("read-only"))
    )
    audit.record(audit.PAIRING_STATE_CHANGED, audit.ACTOR_OPERATOR, {"role": "gs"})
    assert not blocked.exists()


def test_a_non_serializable_detail_still_records(audit, tmp_path) -> None:
    class Opaque:
        def __repr__(self) -> str:
            return "<opaque>"

    audit.record(audit.PAIRING_STATE_CHANGED, audit.ACTOR_OPERATOR, {"x": Opaque()})
    rec = _records(tmp_path / "audit.jsonl")[0]
    assert rec["detail"]["x"] == "<opaque>", (
        "a detail that will not serialize must degrade to its repr, not drop the "
        "whole decision"
    )


def test_applying_a_regulatory_posture_records_exactly_one_decision(
    audit, tmp_path, monkeypatch
) -> None:
    """The end-to-end check: the surface that owns region writes the trail."""
    from ados.core.config import ADOSConfig
    from ados.setup.models import RegulatoryApplyRequest
    from ados.setup.profile import apply_regulatory

    config = ADOSConfig()

    class _Raw:
        def save_config(self) -> None:
            pass

    class _Runtime:
        def __init__(self) -> None:
            self.config = config
            self.raw_runtime = _Raw()

    result = apply_regulatory(_Runtime(), RegulatoryApplyRequest(mode="region", region="IN"))
    assert result.ok

    recs = _records(tmp_path / "audit.jsonl")
    assert len(recs) == 1, recs
    assert recs[0]["kind"] == "regulatory.posture_applied"
    assert recs[0]["actor"] == "operator"
    assert recs[0]["detail"]["region"] == "IN"
    assert recs[0]["detail"]["mode"] == "region"
    assert recs[0]["detail"]["restart_required"] is True

    # Idempotent: re-applying the same posture is not a decision.
    apply_regulatory(_Runtime(), RegulatoryApplyRequest(mode="region", region="IN"))
    assert len(_records(tmp_path / "audit.jsonl")) == 1


def test_the_writer_and_the_janitor_name_the_same_file() -> None:
    """The cross-language contract nothing else checks.

    The trimmer lives in Rust (`ados-supervisor`'s disk janitor) and hardcodes its
    own literal; the writer is Python and resolves a constant. Nothing links them
    at compile time, so a rename on either side leaves a trail that grows without
    a cap and a janitor trimming a file that does not exist — the exact split that
    let this file be budgeted for so long without being written.
    """
    janitor = (
        REPO_ROOT / "crates" / "ados-supervisor" / "src" / "janitor" / "mod.rs"
    ).read_text()
    match = re.search(r'audit_log:\s*PathBuf::from\("([^"]+)"\)', janitor)
    assert match, (
        "could not find the janitor's audit_log path literal; the parser is broken, "
        "so this comparison proves nothing"
    )

    # Compare the LINUX resolution: the janitor only ever runs on a target, while
    # the Python constant also has a macOS-workstation branch. Reloading under a
    # forced platform is what makes the two comparable from a dev machine.
    import platform as platform_mod

    real_system = platform_mod.system
    platform_mod.system = lambda: "Linux"
    try:
        import ados.core.paths as paths

        paths = importlib.reload(paths)
        linux_path = str(paths.AUDIT_LOG)
    finally:
        platform_mod.system = real_system
        importlib.reload(paths)

    assert linux_path == match.group(1), (
        f"the Python writer targets {linux_path} while the Rust janitor trims "
        f"{match.group(1)}: the trail would grow uncapped and the trimmer would "
        "cut nothing"
    )
