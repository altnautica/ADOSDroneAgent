# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""The operator audit trail: an append-only record of decisions that stick.

``/var/ados/audit.jsonl`` was budgeted, trimmed and rendered before anything
wrote it. The supervisor's disk janitor gives it a size cap, trims it on a rung,
counts it toward the agent's footprint, and ``ados diag`` renders it as "audit
trail" — all of a file that never existed. A category the operator can see and
nothing produces is the producer-with-no-reader defect inverted, and it is worse
than absence: it implies a trail exists.

What the readers actually constrain is only two things — the file is APPEND-ONLY
and NEWLINE-DELIMITED, because ``reclaim::trim_append_only`` cuts on a record
boundary. Nothing parses a field. The record shape below is therefore this
module's to define, and it matches the logd event convention the rest of the
agent uses.

## What belongs here, and what does not

This is not a log. Structured logs go to ``ados-logd``, which has retention, a
query API, and far more volume. The audit trail answers one question an operator
asks after the fact: *who changed what, and when*. So it records DECISIONS THAT
PERSIST — a posture written to config, a capability granted to a plugin, a
credential-bearing pair transition, a sandbox rule enforced against a plugin that
tried to step outside it. Four kinds today, each one a decision that outlives the
process that made it.

## Failure posture

A write failure is never fatal to the action being recorded. Refusing an operator
a regulatory change because a disk is full would be a worse outcome than the
missing line; the janitor already guarantees the file cannot grow without bound.
"""

from __future__ import annotations

import json
import time
from typing import Any

from ados.core.logging import get_logger
from ados.core.paths import AUDIT_LOG

log = get_logger("core.audit")

#: The decision kinds this trail records. Kept as an explicit set so a typo lands
#: as a distinct kind nobody greps for rather than silently joining the stream.
REGULATORY_POSTURE_APPLIED = "regulatory.posture_applied"
PLUGIN_PERMISSIONS_GRANTED = "plugin.permissions_granted"
PLUGIN_SANDBOX_VIOLATION = "plugin.sandbox_violation"
PAIRING_STATE_CHANGED = "pairing.state_changed"

KINDS = (
    REGULATORY_POSTURE_APPLIED,
    PLUGIN_PERMISSIONS_GRANTED,
    PLUGIN_SANDBOX_VIOLATION,
    PAIRING_STATE_CHANGED,
)

#: Who made the decision. `operator` is a human action arriving over an
#: authenticated surface; `service` is the agent enforcing a rule on its own.
ACTOR_OPERATOR = "operator"
ACTOR_SERVICE = "service"


def record(kind: str, actor: str, detail: dict[str, Any]) -> None:
    """Append one decision to the audit trail.

    Writes a single newline-terminated JSON object:
    ``{"ts": <unix_ms>, "kind": ..., "actor": ..., "detail": {...}}``.

    Never raises. An unwritable path, a full disk, or a detail that will not
    serialize is logged at warning and dropped — the caller's action stands.
    """
    try:
        line = json.dumps(
            {
                "ts": int(time.time() * 1000),
                "kind": kind,
                "actor": actor,
                "detail": detail,
            },
            separators=(",", ":"),
            default=str,
        )
    except (TypeError, ValueError) as exc:  # pragma: no cover - default=str covers it
        log.warning("audit_record_not_serializable", kind=kind, error=str(exc))
        return

    try:
        AUDIT_LOG.parent.mkdir(parents=True, exist_ok=True)
        # One `write` of one line: the janitor trims on newline boundaries, and
        # append mode on a single small write is what keeps a concurrent writer
        # from interleaving mid-record.
        with AUDIT_LOG.open("a", encoding="utf-8") as fh:
            fh.write(line + "\n")
            fh.flush()
    except OSError as exc:
        log.warning("audit_record_write_failed", kind=kind, error=str(exc))
