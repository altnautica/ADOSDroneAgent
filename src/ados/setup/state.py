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
    # Operator clicked "Skip to Home" — the wizard is dismissed without
    # being finalized. Distinct from ``setup_finalized`` so we can show
    # a resume banner on Home and still let the wizard reach Finish
    # later. The webapp routes to Home when EITHER flag is true.
    setup_skipped: bool = False
    skipped_steps: set[str] = field(default_factory=set)
    # Step ids that have been observed in the "complete" state at any
    # point in the past. Used to keep the setup percentage monotonic
    # across transient state flips: once an operator has completed
    # `pair` or `mavlink` the percentage no longer drops because the
    # FC heartbeat is briefly absent post-reboot.
    ever_completed_steps: set[str] = field(default_factory=set)
    # Sticky acknowledgement bits keyed by short id. The dashboard
    # surfaces a one-shot prompt the first time an operator lands on
    # Home with a posture or feature that warrants it (e.g. cloud
    # posture default flipped to local-only on new installs); once
    # the operator dismisses or acts on the prompt the id lands here
    # and the prompt suppresses on every subsequent load.
    acked_nudges: set[str] = field(default_factory=set)

    def to_dict(self) -> dict:
        return {
            "setup_finalized": bool(self.setup_finalized),
            "setup_skipped": bool(self.setup_skipped),
            "skipped_steps": sorted(self.skipped_steps),
            "ever_completed_steps": sorted(self.ever_completed_steps),
            "acked_nudges": sorted(self.acked_nudges),
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


# Legitimate step ids the wizard can persist as skipped. Anything else
# in skipped_steps is dropped on load so old state files written by a
# wizard that has since dropped a step (e.g. the read-only network
# readout) do not pollute the in-memory set forever.
_KNOWN_STEP_IDS: frozenset[str] = frozenset(
    {
        "welcome",
        "profile",
        "hardware_check",
        "navigation",
        "cloud_choice",
        "pair",
        "mavlink",
        "video",
        "ground_receiver",
        "remote_access",
        "finish",
    }
)


def read_state() -> SetupRunState:
    """Read the persisted state from disk.

    Returns a populated :class:`SetupRunState`. A missing or corrupt
    file resolves to defaults. Step ids that no longer exist are
    silently dropped so a wizard that retired a step never has to
    cope with stale entries.
    """
    raw = _read_raw()
    skipped = raw.get("skipped_steps") or []
    if not isinstance(skipped, list):
        skipped = []
    cleaned_skipped = {
        str(s) for s in skipped if isinstance(s, str) and str(s) in _KNOWN_STEP_IDS
    }
    ever = raw.get("ever_completed_steps") or []
    if not isinstance(ever, list):
        ever = []
    cleaned_ever = {
        str(s) for s in ever if isinstance(s, str) and str(s) in _KNOWN_STEP_IDS
    }
    nudges = raw.get("acked_nudges") or []
    if not isinstance(nudges, list):
        nudges = []
    cleaned_nudges = {str(s) for s in nudges if isinstance(s, str)}
    return SetupRunState(
        setup_finalized=bool(raw.get("setup_finalized", False)),
        setup_skipped=bool(raw.get("setup_skipped", False)),
        skipped_steps=cleaned_skipped,
        ever_completed_steps=cleaned_ever,
        acked_nudges=cleaned_nudges,
    )


def ack_nudge(nudge_id: str) -> SetupRunState:
    """Mark ``nudge_id`` as acknowledged so the dashboard suppresses
    it on every future load. Persisting is best-effort: a read-only
    filesystem returns the in-memory state without raising so the
    caller can still flip its UI."""
    state = read_state()
    state.acked_nudges.add(nudge_id)
    try:
        _write(state)
    except OSError:
        pass
    return state


def _write(state: SetupRunState) -> None:
    SETUP_STATE_DIR.mkdir(parents=True, exist_ok=True)
    tmp = SETUP_STATE_PATH.with_suffix(SETUP_STATE_PATH.suffix + ".tmp")
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(state.to_dict(), fh, sort_keys=True)
        fh.write("\n")
    os.replace(tmp, SETUP_STATE_PATH)


def mark_finalized() -> SetupRunState:
    """Record that the operator clicked Finish in the wizard.

    Finalizing also clears any "Skip to Home" dismissal so the resume
    banner disappears.
    """
    state = read_state()
    state.setup_finalized = True
    state.setup_skipped = False
    _write(state)
    return state


def mark_setup_skipped() -> SetupRunState:
    """Record that the operator dismissed the wizard via Skip to Home.

    Distinct from ``mark_finalized``: the wizard is not finished, but
    Home is reachable. The dashboard renders a resume banner so the
    operator can pick the wizard back up at any time.
    """
    state = read_state()
    state.setup_skipped = True
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


def record_ever_complete(step_ids: set[str]) -> SetupRunState:
    """Promote any newly-complete step ids to the persisted set.

    Returns the post-write state. The write only happens when the set
    actually grows so that hot-path reads don't burn IOs on every
    setup-status request. Persistence failures (read-only filesystem,
    missing dir on a dev box) degrade silently to in-memory-only: the
    caller still gets the unioned set so the current request's
    percentage calc is correct, the next request will retry the write.
    """
    state = read_state()
    promoted = step_ids - state.ever_completed_steps
    if not promoted:
        return state
    state.ever_completed_steps |= promoted
    try:
        _write(state)
    except OSError:
        # Persistence is best-effort. The in-memory set still flows
        # back to the caller via the return value below.
        pass
    return state
