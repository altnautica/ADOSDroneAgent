"""Configuration routes."""

from __future__ import annotations

from fastapi import APIRouter
from pydantic import BaseModel

from ados.api.deps import get_agent_app

router = APIRouter()


@router.get("/config")
async def get_config():
    """Current config (sanitized, no secrets)."""
    app = get_agent_app()
    data = app.config.model_dump()
    # Redact secrets
    if "security" in data:
        sec = data["security"]
        if "tls" in sec:
            sec["tls"]["key_path"] = "***"
        if "api" in sec:
            sec["api"]["api_key"] = "***" if sec["api"].get("api_key") else ""
        if "wireguard" in sec:
            sec["wireguard"]["config_path"] = "***"
    if "server" in data and "self_hosted" in data["server"]:
        data["server"]["self_hosted"]["api_key"] = "***"
    return data


class ConfigUpdate(BaseModel):
    key: str
    value: str


@router.put("/config")
async def update_config(update: ConfigUpdate):
    """Update a config value (dot-separated key path)."""
    app = get_agent_app()
    parts = update.key.split(".")

    # Navigate to the parent object
    obj = app.config
    for part in parts[:-1]:
        if hasattr(obj, part):
            obj = getattr(obj, part)
        else:
            return {"error": f"Key not found: {update.key}"}

    last = parts[-1]
    if not hasattr(obj, last):
        return {"error": f"Key not found: {update.key}"}

    # Try to cast to the correct type
    current = getattr(obj, last)
    try:
        if isinstance(current, bool):
            val = update.value.lower() in ("true", "1", "yes")
        elif isinstance(current, int):
            val = int(update.value)
        elif isinstance(current, float):
            val = float(update.value)
        else:
            val = update.value
        setattr(obj, last, val)
    except (ValueError, TypeError) as e:
        return {"error": f"Invalid value: {e}"}

    return {"status": "ok", "key": update.key, "value": val}


class ConfigSetBody(BaseModel):
    key: str
    value: object


@router.post("/config/set")
async def config_set(body: ConfigSetBody):
    """POST /api/config/set — used by MCP config.set tool."""
    # Delegate to PUT /config with string coerced value
    update = ConfigUpdate(key=body.key, value=str(body.value))
    return await update_config(update)


@router.get("/config/validate")
async def config_validate():
    """Re-parse config file; return validation errors if any."""
    from ados.core.config import load_config
    try:
        load_config()
        return {"valid": True, "errors": []}
    except Exception as e:
        return {"valid": False, "errors": [str(e)]}


@router.post("/config/apply")
async def config_apply():
    """Write current in-memory config to disk and send SIGHUP to services.

    v1.0 note: SIGHUP plumbing per service is future work.
    """
    app = get_agent_app()
    import yaml
    from pathlib import Path
    config_path = Path("/etc/ados/config.yaml")
    try:
        config_path.parent.mkdir(parents=True, exist_ok=True)
        config_path.write_text(yaml.safe_dump(app.config.model_dump(), default_flow_style=False))
        return {"ok": True, "path": str(config_path)}
    except OSError as e:
        return {"ok": False, "error": str(e)}


@router.post("/config/reload")
async def config_reload():
    """Reload config from disk."""
    from ados.core.config import load_config
    try:
        app = get_agent_app()
        app.config = load_config()
        return {"ok": True}
    except Exception as e:
        return {"ok": False, "error": str(e)}
