"""Configuration routes."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, ValidationError

from ados.api.deps import get_agent_app
from ados.core.config import SECRET_PATHS

router = APIRouter()


# Every config path carrying a credential value is redacted on GET and
# protected on PUT. The set is the single ``SECRET_PATHS`` list the schema's
# ``x-secret`` markers are generated from, so the read/write surface and the
# schema can never disagree about which fields are secret, and a newly
# declared secret field is covered here without a second edit.
_REDACTED_SENTINEL = "***"


def _redact_secret_paths(data: dict[str, Any]) -> None:
    """Overwrite every secret VALUE in the config dump with the sentinel.

    Driven by ``SECRET_PATHS`` so a config read surface can never expose a
    credential. An empty/unset value is left untouched: the read stays
    truthful about set-vs-not-set (the ``x-secret`` UI intent) and there is no
    secret to hide.
    """
    for dotted in SECRET_PATHS:
        parts = dotted.split(".")
        node: Any = data
        for part in parts[:-1]:
            if not isinstance(node, dict):
                node = None
                break
            node = node.get(part)
        if isinstance(node, dict):
            leaf = parts[-1]
            if node.get(leaf):
                node[leaf] = _REDACTED_SENTINEL


@router.get("/config")
async def get_config():
    """Current config (sanitized, no secrets)."""
    app = get_agent_app()
    data = app.config.model_dump()
    # Surface the forced-board slug the HAL detector reads from the
    # /etc/ados/board_override file. It is a file, not a config field, so
    # it is not in model_dump(); expose it under agent.board_override so a
    # UI that sets it can read it back. Empty string = auto-detect.
    from ados.setup.advanced import read_board_override

    if isinstance(data.get("agent"), dict):
        data["agent"]["board_override"] = read_board_override()
    _redact_secret_paths(data)
    return data


class ConfigUpdate(BaseModel):
    key: str
    value: str


@router.put("/config")
async def update_config(update: ConfigUpdate):
    """Update a config value (dot-separated key path).

    Mutates the in-memory Pydantic model AND persists to
    `/etc/ados/config.yaml` via `app.save_config()`. Without the disk
    write, the next `systemctl restart ados-*` reloads defaults and
    silently undoes the update.

    Rejects writes targeting redacted secret paths when the caller
    submits the `***` sentinel returned by GET; this prevents a
    GET -> display -> edit-other-field -> PUT flow from corrupting a
    real secret with the placeholder.

    Validates the mutated Pydantic parent against its own model class
    before writing to disk; without this, a value that passes the
    in-memory type cast but violates a custom Pydantic validator
    would land in YAML and crash the agent on next restart.
    """
    app = get_agent_app()

    # Block writes that would corrupt a secret with the GET redaction sentinel.
    if update.key in SECRET_PATHS and update.value == _REDACTED_SENTINEL:
        raise HTTPException(
            status_code=400,
            detail={
                "error": {
                    "code": "E_REDACTED_SENTINEL",
                    "message": (
                        f"Refusing to write the redaction sentinel "
                        f"'{_REDACTED_SENTINEL}' to secret path "
                        f"'{update.key}'. Submit the real value or omit "
                        f"this field from the PUT."
                    ),
                }
            },
        )

    parts = update.key.split(".")

    # Walk the live Pydantic config to find both the parent and the leaf.
    parent = app.config
    for part in parts[:-1]:
        if hasattr(parent, part):
            parent = getattr(parent, part)
        else:
            return {"error": f"Key not found: {update.key}"}

    last = parts[-1]
    if not hasattr(parent, last):
        return {"error": f"Key not found: {update.key}"}

    current = getattr(parent, last)
    try:
        if isinstance(current, bool):
            val = update.value.lower() in ("true", "1", "yes")
        elif isinstance(current, int):
            val = int(update.value)
        elif isinstance(current, float):
            val = float(update.value)
        else:
            val = update.value
    except (ValueError, TypeError) as e:
        return {"error": f"Invalid value: {e}"}

    # Re-validate the parent section through its own Pydantic class
    # before mutating the live model. If a custom validator rejects the
    # value, surface a 422 instead of letting the bad value flow into
    # the on-disk YAML where it would crash the agent on next restart.
    parent_cls = type(parent)
    try:
        snapshot = parent.model_dump()
        snapshot[last] = val
        parent_cls(**snapshot)
    except ValidationError as e:
        raise HTTPException(
            status_code=422,
            detail={
                "error": {
                    "code": "E_VALIDATION",
                    "key": update.key,
                    "messages": [str(err) for err in e.errors()],
                }
            },
        ) from e
    except Exception:  # noqa: BLE001
        # Defensive: model_dump should never raise on a valid Pydantic
        # instance, but if it does (custom __get_pydantic_core_schema__
        # quirks, etc.) we'd rather skip the strict validation than
        # 500.
        pass

    setattr(parent, last, val)

    # Persist via the runtime's save_config helper, which is now the
    # single point of contention for /etc/ados/config.yaml writes.
    persisted = False
    try:
        persisted = bool(app.save_config())
    except Exception as exc:  # noqa: BLE001
        return {
            "status": "ok",
            "key": update.key,
            "value": val,
            "persisted": False,
            "persist_error": str(exc),
        }

    return {
        "status": "ok",
        "key": update.key,
        "value": val,
        "persisted": persisted,
    }
