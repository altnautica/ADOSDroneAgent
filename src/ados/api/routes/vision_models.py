# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""Vision model registry + download API routes.

Provides:
  - GET  /api/vision/models                          list registry + installed + custom + active + cache usage
  - POST /api/vision/models/{model_id}/download     pick the best variant for this board
  - GET  /api/vision/models/{model_id}/status       download progress + installed state
  - POST /api/vision/plugin-models/{plugin_id}/deliver
                                                    resolve + cache the models an installed plugin declares

The model cache is decoupled from any specific autonomy feature; plugins
that need an inference model load via the same registry.

The active-detector selection and the custom-model upload are control-plane
writes served by the native front; this read route projects them back so the
GCS sees which model is active and lists operator-uploaded models alongside the
registry and the on-disk files.
"""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import Any

import yaml
from fastapi import APIRouter, HTTPException
from fastapi.responses import JSONResponse

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger
from ados.core.paths import CONFIG_YAML, PLUGINS_INSTALL_DIR
from ados.plugins.errors import ManifestError
from ados.plugins.manifest import PLUGIN_ID_PATTERN, PluginManifest

log = get_logger("api.vision_models")

router = APIRouter()


def _active_detector_model_id() -> str | None:
    """The currently-selected detector model id, read straight from the config.

    The native ``PUT /api/vision/detector`` route writes ``vision.detector`` into
    ``/etc/ados/config.yaml`` (a block the Python ``VisionConfig`` model does not
    declare, so it is dropped on the typed load); the raw YAML is the source of
    truth. A missing file / block / id yields ``None``.
    """
    try:
        with open(str(CONFIG_YAML), encoding="utf-8") as fh:
            data = yaml.safe_load(fh)
    except (OSError, yaml.YAMLError):
        return None
    if not isinstance(data, dict):
        return None
    vision = data.get("vision")
    if not isinstance(vision, dict):
        return None
    detector = vision.get("detector")
    if not isinstance(detector, dict):
        return None
    model_id = detector.get("model_id")
    return model_id if isinstance(model_id, str) and model_id else None


@router.get("/vision/models")
async def list_vision_models() -> dict[str, Any]:
    """List available, installed, and custom vision models plus the active one."""
    active = _active_detector_model_id()
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        return {
            "registry": [],
            "installed": [],
            "custom": [],
            "active": active,
            "cache": {"used_bytes": 0, "max_bytes": 0, "used_mb": 0, "max_mb": 0},
        }

    # Refresh registry (uses ETag caching, fast on 304)
    await mm.fetch_registry()

    return {
        "registry": [m.to_dict() for m in mm.registry],
        "installed": mm.list_installed(),
        "custom": mm.list_custom(),
        "active": active,
        "cache": mm.get_cache_usage(),
    }


# Model downloads in flight, keyed by model id. Held so the task survives the
# request that started it, and so a second request for the same model is
# refused instead of racing the first on the same temp file.
_DOWNLOADS: dict[str, asyncio.Task[str]] = {}


def _download_finished(model_id: str, task: asyncio.Task[str]) -> None:
    """Drop a finished download and log how it ended (progress keeps the error)."""
    if _DOWNLOADS.get(model_id) is task:
        del _DOWNLOADS[model_id]
    if task.cancelled():
        log.warning("model_download_cancelled", model=model_id)
        return
    exc = task.exception()
    if exc is not None:
        log.warning("model_download_task_failed", model=model_id, error=str(exc))


@router.post("/vision/models/{model_id}/download", status_code=202)
async def download_vision_model(model_id: str) -> dict[str, Any]:
    """Start downloading a vision model, picking the best variant for this board.

    The download runs in the background, past the request's lifetime: a large
    model takes longer than any proxy's request timeout. The reply is 202 with
    the status URL to poll; 409 when the same model is already downloading.
    """
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        raise HTTPException(status_code=503, detail="Model manager not available")
    await mm.fetch_registry()
    running = _DOWNLOADS.get(model_id)
    if running is not None and not running.done():
        raise HTTPException(status_code=409, detail=f"Model {model_id} is already downloading")

    variant = mm.select_best_variant(model_id)
    if not variant:
        raise HTTPException(status_code=404, detail=f"No suitable variant found for model: {model_id}")
    if not variant.get("url"):
        raise HTTPException(status_code=422, detail=f"No download URL for model variant: {model_id}")

    task = asyncio.create_task(mm.download_model(model_id))
    _DOWNLOADS[model_id] = task
    task.add_done_callback(lambda t: _download_finished(model_id, t))
    return {
        "status": "started",
        "model_id": model_id,
        "status_url": f"/api/vision/models/{model_id}/status",
    }


@router.get("/vision/models/{model_id}/status")
async def get_model_status(model_id: str) -> dict[str, Any]:
    """Get download progress and installed status for a model."""
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if mm is None:
        return {"installed": False, "download": None}

    installed = False
    for m in mm.list_installed():
        if m["id"] == model_id:
            installed = True
            break

    progress = mm.get_download_progress(model_id)
    return {
        "installed": installed,
        "download": progress.to_dict(),
    }


def _err(code: int, kind: str, detail: str, status: int) -> JSONResponse:
    """The plugin-lifecycle error envelope the native caller maps."""
    return JSONResponse(
        {"ok": False, "code": code, "kind": kind, "detail": detail}, status_code=status
    )


def _installed_manifest_path(plugin_id: str) -> Path:
    """``<install dir>/<id>/manifest.yaml``, honouring the plugin host's install-dir override."""
    install_dir = os.environ.get("ADOS_PLUGIN_INSTALL_DIR") or str(PLUGINS_INSTALL_DIR)
    return Path(install_dir) / plugin_id / "manifest.yaml"


def _board_hint(app: Any) -> str | None:
    """Board name, SoC and model joined, so ``board_family`` can match any of them.

    The SoC is the reliable token: display names ("Orange Pi 5", "Raspberry Pi
    CM5") vary by vendor, while ``rk3588``/``bcm2712``/``a733`` do not.
    """
    parts = (
        getattr(app, "board_name", None),
        getattr(app, "board_soc", None),
        getattr(app, "board_model", None),
    )
    return " ".join(str(x) for x in parts if x) or None


@router.post("/vision/plugin-models/{plugin_id}/deliver", response_model=None)
async def deliver_plugin_models(plugin_id: str) -> dict[str, Any] | JSONResponse:
    """Resolve + cache the board-appropriate variant of every model a plugin declares.

    Called by the native plugin lifecycle right after it enables a plugin, so a
    plugin that references a model gets it delivered with no operator step:
    board-match select, then a verified cache hit, else fetch + sha256 verify,
    else ``needs_model`` with a reason. The caller persists the returned list as
    the install's ``model_status`` (the heartbeat and the host's
    ``vision.read_model`` read it from there); this route writes no plugin state.
    The plugin itself registers the delivered path with the engine through the
    capability-gated SDK ``register_model``.
    """
    if not PLUGIN_ID_PATTERN.match(plugin_id):
        return _err(2, "usage_error", f"invalid plugin id {plugin_id!r}", 400)
    manifest_path = _installed_manifest_path(plugin_id)
    if not manifest_path.is_file():
        return _err(14, "not_found", f"plugin {plugin_id} not installed", 404)
    try:
        manifest = PluginManifest.from_yaml_file(manifest_path)
    except ManifestError as exc:
        return _err(12, "manifest_invalid", str(exc), 400)
    vision = manifest.agent.contributes.vision if manifest.agent is not None else None
    refs = vision.model_refs() if vision is not None else []
    app = get_agent_app()
    mm = getattr(app, "model_manager", None)
    if not refs or mm is None:
        return {"ok": True, "plugin_id": plugin_id, "models": []}
    resolutions = await mm.resolve_plugin_models(refs, _board_hint(app))
    models = [r.to_dict() for r in resolutions]
    log.info(
        "plugin_models_delivered",
        plugin_id=plugin_id,
        total=len(models),
        needs_model=sum(1 for m in models if m.get("state") != "resolved"),
    )
    return {"ok": True, "plugin_id": plugin_id, "models": models}
