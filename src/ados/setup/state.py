"""Persistent setup state for the onboarding gate.

Tracks two pieces of operator intent that are not derivable from runtime
state:

* ``setup_finalized`` — operator has explicitly clicked Finish in the
  onboarding wizard. Until this is true the universal webapp gates the
  rest of the app surface and forces the user back into the wizard.
* ``skipped_steps`` — set of step ids the operator chose to defer with
  "Skip for now". The setup status assembler downgrades those steps
  from ``needs_action`` to ``optional`` so they no longer block the
  ``setup_complete`` derivation.

Stored as JSON at :data:`ados.core.paths.SETUP_STATE_PATH`. The file is
created on first write; reads from a missing file return defaults.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field

from ados.core.paths import SETUP_STATE_DIR, SETUP_STATE_PATH


@dataclass
class SetupRunState:
    """In-memory view of the on-disk setup state."""

    setup_finalized: bool = False
    skipped_steps: set[str] = field(default_factory=set)

    def to_dict(self) -> dict:
        return {
            "setup_finalized": bool(self.setup_finalized),
            "skipped_steps": sorted(self.skipped_steps),
        }


def _read_raw() -> dict:
    try:
        with open(SETUP_STATE_PATH, encoding="utf-8") as fh:
            data = json.load(fh)
    except FileNotFoundError:
        return {}
    except (OSError, json.JSONDecodeError):
        # Corrupt file: ignore and let the caller see defaults. The next
        # write will overwrite cleanly.
        return {}
    return data if isinstance(data, dict) else {}


def read_state() -> SetupRunState:
    """Read the persisted state from disk.

    Returns a populated :class:`SetupRunState`. A missing or corrupt
    file resolves to defaults.
    """
    raw = _read_raw()
    skipped = raw.get("skipped_steps") or []
    if not isinstance(skipped, list):
        skipped = []
    return SetupRunState(
        setup_finalized=bool(raw.get("setup_finalized", False)),
        skipped_steps={str(s) for s in skipped if isinstance(s, str)},
    )


def _write(state: SetupRunState) -> None:
    SETUP_STATE_DIR.mkdir(parents=True, exist_ok=True)
    tmp = SETUP_STATE_PATH.with_suffix(SETUP_STATE_PATH.suffix + ".tmp")
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(state.to_dict(), fh, sort_keys=True)
        fh.write("\n")
    os.replace(tmp, SETUP_STATE_PATH)


def mark_finalized() -> SetupRunState:
    """Record that the operator clicked Finish in the wizard."""
    state = read_state()
    state.setup_finalized = True
    _write(state)
    return state


def mark_skipped(step_id: str) -> SetupRunState:
    """Record that the operator chose Skip for ``step_id``."""
    state = read_state()
    state.skipped_steps.add(step_id)
    _write(state)
    return state


def clear_skipped(step_id: str) -> SetupRunState:
    """Reverse a skip when the operator engages the step again."""
    state = read_state()
    state.skipped_steps.discard(step_id)
    _write(state)
    return state


def reset_state() -> SetupRunState:
    """Forget finalization and skipped steps. Used by Re-run setup."""
    state = SetupRunState()
    _write(state)
    return state
