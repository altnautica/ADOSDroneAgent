"""Configuration routes."""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel, ValidationError

from ados.api.deps import get_agent_app

router = APIRouter()


# Dot-paths the GET response redacts. PUT rejects writes to these paths
# unless the caller provides something other than the redaction sentinel,
# so a GET-then-PUT round trip cannot corrupt the real secret with the
# literal "***" placeholder.
_REDACTED_PATHS: tuple[str, ...] = (
    "security.tls.key_path",
    "security.api.api_key",
    "security.wireguard.config_path",
    "server.self_hosted.api_key",
)
_REDACTED_SENTINEL = "***"


@router.get("/config")
async def get_config():
    """Current config (sanitized, no secrets)."""
    app = get_agent_app()
    data = app.config.model_dump()
    # Redact secrets
    if "security" in data:
        sec = data["security"]
        if "tls" in sec:
            sec["tls"]["key_path"] = _REDACTED_SENTINEL
        if "api" in sec:
            sec["api"]["api_key"] = (
                _REDACTED_SENTINEL if sec["api"].get("api_key") else ""
            )
        if "wireguard" in sec:
            sec["wireguard"]["config_path"] = _REDACTED_SENTINEL
    if "server" in data and "self_hosted" in data["server"]:
        data["server"]["self_hosted"]["api_key"] = _REDACTED_SENTINEL
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
    if update.key in _REDACTED_PATHS and update.value == _REDACTED_SENTINEL:
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
