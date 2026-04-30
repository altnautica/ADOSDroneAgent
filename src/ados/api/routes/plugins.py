"""Plugin lifecycle REST routes.

Operator-facing surface called by the GCS Settings -> Plugins page.
The Python-side supervisor at :class:`ados.plugins.supervisor.PluginSupervisor`
does the work; these routes are thin adapters that translate
HTTP -> supervisor calls and surface manifest summaries the GCS can
render.

Endpoints:

* ``GET    /api/plugins``                       list installs
* ``GET    /api/plugins/{plugin_id}``           one install detail + manifest
* ``POST   /api/plugins/install``               multipart upload (.adosplug)
* ``POST   /api/plugins/{plugin_id}/grant``     grant one declared permission
* ``POST   /api/plugins/{plugin_id}/enable``    enable + start
* ``POST   /api/plugins/{plugin_id}/disable``   stop + disable
* ``DELETE /api/plugins/{plugin_id}``           remove (optional ?keep_data=1)

Errors map to the CLI exit-code taxonomy via the structured
``{"ok": false, "code": N, "kind": "...", "detail": "..."}``
JSON envelope. Code numbers match ``ados.cli.plugin``.
"""

from __future__ import annotations

import tempfile
from pathlib import Path

from fastapi import APIRouter, File, HTTPException, UploadFile
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from ados.core.logging import get_logger
from ados.plugins.archive import open_archive
from ados.plugins.errors import (
    ArchiveError,
    ManifestError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.supervisor import PluginSupervisor

log = get_logger("api.plugins")

router = APIRouter()


# Module-level supervisor singleton. Constructed lazily on first call
# so test environments without /var/ados can still import this module.
_supervisor: PluginSupervisor | None = None


def _get_supervisor() -> PluginSupervisor:
    global _supervisor
    if _supervisor is None:
        sup = PluginSupervisor()
        sup.discover()
        _supervisor = sup
    return _supervisor


def _set_supervisor_for_tests(sup: PluginSupervisor | None) -> None:
    """Test seam. The agent main process never calls this."""
    global _supervisor
    _supervisor = sup


# ---------------------------------------------------------------------
# Error envelope
# ---------------------------------------------------------------------


def _err(code: int, kind: str, detail: str, status: int = 400) -> JSONResponse:
    return JSONResponse(
        {"ok": False, "code": code, "kind": kind, "detail": detail},
        status_code=status,
    )


# ---------------------------------------------------------------------
# List + detail
# ---------------------------------------------------------------------


def _install_to_dict(install) -> dict:
    return {
        "plugin_id": install.plugin_id,
        "version": install.version,
        "source": install.source,
        "source_uri": install.source_uri,
        "signer_id": install.signer_id,
        "manifest_hash": install.manifest_hash,
        "status": install.status,
        "installed_at": install.installed_at,
        "enabled_at": install.enabled_at,
        "permissions": {
            pid: {
                "granted": grant.granted,
                "granted_at": grant.granted_at,
            }
            for pid, grant in install.permissions.items()
        },
    }


@router.get("/plugins")
async def list_plugins() -> dict:
    sup = _get_supervisor()
    return {"installs": [_install_to_dict(i) for i in sup.installs()]}


@router.get("/plugins/{plugin_id}")
async def get_plugin(plugin_id: str):
    sup = _get_supervisor()
    install = next(
        (i for i in sup.installs() if i.plugin_id == plugin_id), None
    )
    if install is None:
        return _err(14, "not_found", f"plugin {plugin_id} not installed", 404)
    try:
        manifest = sup._manifest_for(plugin_id)
    except SupervisorError as exc:
        return _err(20, "host_io_error", str(exc), 500)
    return {
        "install": _install_to_dict(install),
        "manifest": {
            "id": manifest.id,
            "version": manifest.version,
            "name": manifest.name,
            "risk": manifest.risk,
            "license": manifest.license,
            "halves": (
                ["agent"] if manifest.agent is not None else []
            )
            + (["gcs"] if manifest.gcs is not None else []),
            "permissions": [
                {"id": pid, "required": True}
                for pid in sorted(manifest.declared_permissions())
            ],
        },
    }


# ---------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------


@router.post("/plugins/install")
async def install_plugin(file: UploadFile = File(...)):
    """Multipart upload of a ``.adosplug`` archive.

    The archive is read into a temp file (to keep the supervisor's
    on-disk pathing intact), parsed, signature-verified, and the
    manifest summary is returned. Permission grants come on a
    subsequent ``/grant`` call from the install dialog.
    """
    if not file.filename or not file.filename.endswith(".adosplug"):
        return _err(2, "usage_error", "expected a .adosplug file", 400)
    raw = await file.read()
    if not raw:
        return _err(2, "usage_error", "empty upload", 400)
    sup = _get_supervisor()
    with tempfile.NamedTemporaryFile(
        suffix=".adosplug", delete=False
    ) as tmp:
        tmp.write(raw)
        tmp_path = Path(tmp.name)
    try:
        result = sup.install_archive(tmp_path)
    except SignatureError as exc:
        kind_to_code = {
            "missing": 10,
            "invalid": 10,
            "revoked": 10,
            "unknown_signer": 10,
        }
        return _err(
            kind_to_code.get(exc.kind, 10),
            f"signature_{exc.kind}",
            str(exc),
            400,
        )
    except ManifestError as exc:
        return _err(12, "manifest_invalid", str(exc), 400)
    except ArchiveError as exc:
        return _err(12, "archive_invalid", str(exc), 400)
    except SupervisorError as exc:
        msg = str(exc)
        if "ADOS version" in msg:
            return _err(17, "ados_version_skew", msg, 409)
        return _err(20, "host_io_error", msg, 500)
    finally:
        try:
            tmp_path.unlink(missing_ok=True)
        except OSError as exc:
            log.warning("plugin_install_temp_cleanup", error=str(exc))
    return {
        "ok": True,
        "plugin_id": result.plugin_id,
        "version": result.version,
        "signer_id": result.signer_id,
        "risk": result.risk,
        "permissions_requested": result.permissions_requested,
    }


# ---------------------------------------------------------------------
# Lifecycle
# ---------------------------------------------------------------------


class GrantRequest(BaseModel):
    permission_id: str


@router.post("/plugins/{plugin_id}/grant")
async def grant_permission(plugin_id: str, body: GrantRequest):
    sup = _get_supervisor()
    try:
        sup.grant_permission(plugin_id, body.permission_id)
    except SupervisorError as exc:
        msg = str(exc)
        if "did not declare" in msg or "not declared" in msg:
            return _err(11, "permission_deny", msg, 400)
        if "not installed" in msg:
            return _err(14, "not_found", msg, 404)
        return _err(20, "host_io_error", msg, 500)
    return {"ok": True}


@router.post("/plugins/{plugin_id}/enable")
async def enable_plugin(plugin_id: str):
    sup = _get_supervisor()
    try:
        sup.enable(plugin_id)
    except SupervisorError as exc:
        if "not installed" in str(exc):
            return _err(14, "not_found", str(exc), 404)
        return _err(20, "host_io_error", str(exc), 500)
    return {"ok": True}


@router.post("/plugins/{plugin_id}/disable")
async def disable_plugin(plugin_id: str):
    sup = _get_supervisor()
    try:
        sup.disable(plugin_id)
    except SupervisorError as exc:
        if "not installed" in str(exc):
            return _err(14, "not_found", str(exc), 404)
        return _err(20, "host_io_error", str(exc), 500)
    return {"ok": True}


@router.delete("/plugins/{plugin_id}")
async def remove_plugin(plugin_id: str, keep_data: bool = False):
    sup = _get_supervisor()
    try:
        sup.remove(plugin_id, keep_data=keep_data)
    except SupervisorError as exc:
        if "not installed" in str(exc):
            return _err(14, "not_found", str(exc), 404)
        return _err(20, "host_io_error", str(exc), 500)
    return {"ok": True}
