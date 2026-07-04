"""Advanced section setter for the batch-apply route.

Persists the two advanced controls the operator can change:

* ``log_level`` is written to ``config.logging.level`` (the field every
  service reads through ``configure_logging`` at start) and saved to
  ``/etc/ados/config.yaml`` via the runtime's ``save_config``. It takes
  effect the next time a service (re)starts.
* ``board_override`` is written to ``/etc/ados/board_override`` (the file
  ``ados.hal.detect`` reads to force a HAL board profile). Clearing it
  removes the file so detection falls back to auto. The config directory
  honors the ``ADOS_ETC_DIR`` environment override for test/dev sandboxes,
  matching the install scripts.

A persist failure is surfaced as ``ok=False`` rather than swallowed, so a
save that never reached disk is never reported as success.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

from ados.core.paths import ADOS_ETC_DIR
from ados.setup.models import AdvancedApplyRequest, SetupActionResult

_VALID_LOG_LEVELS: frozenset[str] = frozenset(
    {"debug", "info", "warning", "error", "critical"}
)


def board_override_path() -> Path:
    """Path of the board-override file the HAL detector reads.

    Defaults to ``ADOS_ETC_DIR / "board_override"`` (``/etc/ados`` in
    production, the same file ``ados.hal.detect`` reads). The directory
    honors the ``ADOS_ETC_DIR`` environment override so tests and dev
    sandboxes can redirect the write off the real ``/etc``.
    """
    base = os.environ.get("ADOS_ETC_DIR")
    etc_dir = Path(base) if base else ADOS_ETC_DIR
    return etc_dir / "board_override"


def read_board_override() -> str:
    """Return the current forced-board slug, or ``""`` when unset."""
    path = board_override_path()
    try:
        if path.exists():
            return path.read_text().strip()
    except OSError:
        pass
    return ""


def write_board_override(value: str) -> None:
    """Write (or clear) the board-override file.

    An empty ``value`` removes the file so the HAL detector reverts to
    auto-detect. A non-empty value is written atomically (tmp + replace).
    Raises ``OSError`` on a filesystem failure so the caller can surface
    it instead of reporting a phantom success.
    """
    path = board_override_path()
    if not value:
        try:
            path.unlink()
        except FileNotFoundError:
            pass
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(value + "\n")
    tmp.replace(path)


def apply_advanced(
    runtime: Any,
    request: AdvancedApplyRequest | None,
) -> SetupActionResult:
    """Validate + persist an advanced section update.

    Returns ``ok=True`` with ``data.changed=False`` when the request is
    empty so the batch apply route can iterate sections without
    special-casing absent payloads. Bad input returns ``ok=False`` with a
    structured message; a failed persist also returns ``ok=False``.
    """
    if request is None:
        return SetupActionResult(
            ok=True,
            message="No advanced changes requested.",
            data={"changed": False},
        )

    notes: list[str] = []
    fields: list[str] = []

    config = runtime.config
    logging_cfg = getattr(config, "logging", None)
    config_changed = False

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
        if logging_cfg is None:
            return SetupActionResult(
                ok=False,
                message="Logging configuration is not available on this agent.",
            )
        if str(getattr(logging_cfg, "level", "")).lower() != level:
            logging_cfg.level = level
            config_changed = True
        fields.append("log_level")
        notes.append(f"log level set to {level} (applies on service restart)")

    # Persist the config-backed changes (log_level) to disk. A failed save
    # is surfaced, not swallowed: reporting a persist that never reached
    # /etc/ados/config.yaml as success is the exact bug this fix removes.
    if config_changed:
        saver = getattr(getattr(runtime, "raw_runtime", None), "save_config", None)
        if callable(saver):
            try:
                persisted = bool(saver())
            except Exception as exc:  # noqa: BLE001 (surface, don't swallow)
                return SetupActionResult(
                    ok=False,
                    message=f"Log level not saved: config write failed: {exc}",
                )
            if not persisted:
                return SetupActionResult(
                    ok=False,
                    message="Log level not saved: config could not be written to disk.",
                )

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
        try:
            write_board_override(override)
        except OSError as exc:
            return SetupActionResult(
                ok=False,
                message=f"Could not write board override: {exc}",
            )
        fields.append("board_override")
        if override:
            notes.append(f"board override set to {override}")
        else:
            notes.append("board override cleared")

    data: dict[str, object] = {
        "changed": bool(fields),
        "fields": fields,
    }
    if notes:
        message = "Advanced updated: " + "; ".join(notes) + "."
    else:
        message = "No advanced changes detected."
    return SetupActionResult(ok=True, message=message, data=data)


def _is_valid_board_token(value: str) -> bool:
    """Allow short slugs only. Keeps the override input from carrying
    paths or shell-meaningful characters into the file write.
    """
    if len(value) > 64:
        return False
    for ch in value:
        if not (ch.isalnum() or ch in {"-", "_"}):
            return False
    return True
