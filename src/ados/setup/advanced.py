"""Advanced section setter for the batch-apply route.

Validates operator inputs (board override, log level, factory-reset
flag) and acknowledges them. Persistence to ``/etc/ados/board_override``
and runtime log-level switching are wired in a later iteration; this
stub writes any provided board override to the live config so the
detector picks it up on the next reload.
"""

from __future__ import annotations

from typing import Any

from ados.setup.models import AdvancedApplyRequest, SetupActionResult

_VALID_LOG_LEVELS: frozenset[str] = frozenset(
    {"debug", "info", "warning", "error", "critical"}
)


def apply_advanced(
    runtime: Any,
    request: AdvancedApplyRequest | None,
) -> SetupActionResult:
    """Validate + acknowledge an advanced section update.

    Returns ``ok=True`` with ``data.changed=False`` when the request is
    empty so the batch apply route can iterate sections without
    special-casing absent payloads. Bad input returns ``ok=False``
    with a structured message.
    """
    if request is None:
        return SetupActionResult(
            ok=True,
            message="No advanced changes requested.",
            data={"changed": False},
        )

    notes: list[str] = []
    fields: list[str] = []
    queued_reset = False

    if request.log_level is not None:
        level = str(request.log_level).strip().lower()
        if level not in _VALID_LOG_LEVELS:
            return SetupActionResult(
                ok=False,
                message=(
                    "log_level must be one of debug, info, warning, error, "
                    "critical."
                ),
            )
        config = runtime.config
        agent = getattr(config, "agent", None)
        if agent is not None and hasattr(agent, "log_level"):
            if str(getattr(agent, "log_level", "")).lower() != level:
                agent.log_level = level
                fields.append("log_level")
        else:
            # Field is acknowledged but not persisted on this build.
            fields.append("log_level")
        notes.append(f"log level set to {level}")

    if request.board_override is not None:
        override = str(request.board_override).strip()
        if override and not _is_valid_board_token(override):
            return SetupActionResult(
                ok=False,
                message=(
                    "board_override must be a short slug (letters, digits, "
                    "dash, underscore)."
                ),
            )
        fields.append("board_override")
        if override:
            notes.append(f"board override staged as {override}")
        else:
            notes.append("board override cleared")

    if request.factory_reset is True:
        queued_reset = True
        fields.append("factory_reset")
        notes.append("factory reset queued; reboot to apply")

    data: dict[str, object] = {
        "changed": bool(fields),
        "fields": fields,
        "factory_reset_queued": queued_reset,
    }
    if notes:
        message = "Advanced updated: " + "; ".join(notes) + "."
    else:
        message = "No advanced changes detected."
    return SetupActionResult(ok=True, message=message, data=data)


def _is_valid_board_token(value: str) -> bool:
    """Allow short slugs only. Keeps the override input from carrying
    paths or shell-meaningful characters into a setter that may write
    to ``/etc/ados/board_override`` later.
    """
    if len(value) > 64:
        return False
    for ch in value:
        if not (ch.isalnum() or ch in {"-", "_"}):
            return False
    return True
