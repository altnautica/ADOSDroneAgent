"""Model registry REST API routes.

All endpoints are under /api/models/*.
"""

from __future__ import annotations

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

import structlog

log = structlog.get_logger()
router = APIRouter(prefix="/models", tags=["models"])


def _registry():
    from ados.model_registry.registry import get_registry
    return get_registry()


@router.get("")
async def list_models():
    """List all installed models."""
    reg = _registry()
    return [m.to_dict() for m in reg.list_installed()]


@router.get("/{category}/{name}")
async def get_model(category: str, name: str):
    """Get a single model manifest."""
    reg = _registry()
    m = reg.get(category, name)
    if not m:
        raise HTTPException(status_code=404, detail="Model not found")
    return m.to_dict()


class InstallBody(BaseModel):
    download_url: str
    sha256: str = ""
    version: str = "0.0.1"
    accelerator: str = "cpu"


@router.post("/{category}/{name}/install")
async def install_model(category: str, name: str, body: InstallBody):
    """Install a model from CDN URL."""
    reg = _registry()
    try:
        from ados.model_registry.installer import install_from_url
        manifest = await install_from_url(
            reg, category, name,
            download_url=body.download_url,
            expected_sha256=body.sha256,
            version=body.version,
            accelerator=body.accelerator,
        )
        return {"ok": True, "manifest": manifest.to_dict()}
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


class LocalInstallBody(BaseModel):
    path: str
    version: str = "0.0.1"
    accelerator: str = "cpu"


@router.post("/{category}/{name}/install-local")
async def install_model_local(category: str, name: str, body: LocalInstallBody):
    """Install a model from a local tarball (airgap path)."""
    reg = _registry()
    try:
        from ados.model_registry.installer import install_from_local_tarball
        manifest = install_from_local_tarball(reg, category, name, body.path, body.version, body.accelerator)
        return {"ok": True, "manifest": manifest.to_dict()}
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@router.post("/{category}/{name}/pin")
async def pin_model(category: str, name: str):
    """Pin a model (prevent LRU eviction)."""
    reg = _registry()
    if not reg.pin(category, name):
        raise HTTPException(status_code=404, detail="Model not found")
    return {"ok": True}


@router.get("/usage")
async def disk_usage():
    """Total disk usage of installed models."""
    reg = _registry()
    return {"bytes": reg.total_disk_usage()}
