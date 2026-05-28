"""Plugin lifecycle REST routes.

Operator-facing surface called by the GCS Settings -> Plugins page.
The Python-side supervisor at :class:`ados.plugins.supervisor.PluginSupervisor`
does the work; these routes are thin adapters that translate
HTTP -> supervisor calls and surface manifest summaries the GCS can
render.

Endpoints:

* ``GET    /api/plugins``                       list installs
* ``GET    /api/plugins/{plugin_id}``           one install detail + manifest
* ``POST   /api/plugins/parse``                 multipart upload, manifest preview only
* ``POST   /api/plugins/install``               multipart upload (.adosplug), commits
* ``POST   /api/plugins/{plugin_id}/grant``     grant one declared permission
* ``POST   /api/plugins/{plugin_id}/enable``    enable + start
* ``POST   /api/plugins/{plugin_id}/disable``   stop + disable
* ``DELETE /api/plugins/{plugin_id}``           remove (optional ?keep_data=1)

The two-stage install flow runs ``/parse`` first to validate and
preview the manifest without touching disk, then ``/install`` after
operator consent. Both accept the same multipart payload so the
client can upload once and commit only after approval.

Errors map to the CLI exit-code taxonomy via the structured
``{"ok": false, "code": N, "kind": "...", "detail": "..."}``
JSON envelope. Code numbers match ``ados.cli.plugin``.
"""

from __future__ import annotations

import tempfile
from pathlib import Path

import httpx
from fastapi import (
    APIRouter,
    File,
    UploadFile,
    WebSocket,
    WebSocketDisconnect,
)
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from ados.api.deps import get_agent_app
from ados.api.routes._plugins_helpers import (
    TOKEN_TTL_SECONDS_DEFAULT,
    authenticate_job_websocket,
    compute_granted_caps_for_token,
    job_ticket_store,
    mint_agent_capability_token,
    run_job_progress_stream,
    write_sidecar,
)
from ados.core.logging import get_logger
from ados.plugins.capabilities import get_capability_meta, is_known_capability
from ados.plugins.errors import (
    ArchiveError,
    ManifestError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.install_from_url_impl import (
    DOWNLOAD_CONNECT_TIMEOUT,
    DOWNLOAD_TOTAL_TIMEOUT,
    ArchiveDownloadError,
    ArchiveTooLargeError,
    Sha256MismatchError,
    UrlValidationError,
    stream_archive_to_path,
    validate_install_url,
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
# Capability catalog enrichment
# ---------------------------------------------------------------------


def _enrich_permissions(
    permission_ids: list[str],
    *,
    required_ids: set[str] | None = None,
) -> list[dict]:
    """Build the install-dialog permission entries from raw ids.

    For each id, look up the catalog entry and inline ``label``,
    ``description``, ``category``, ``risk``, and ``risk_reason`` on
    the response object. ``required_ids`` controls the ``required``
    flag; when ``None`` every entry defaults to ``True`` because
    today's manifest schema does not distinguish required from
    optional declarations.

    Unknown capability ids are not enriched here; callers must run
    :func:`_unknown_capabilities` first and surface a 400 if any are
    found.
    """
    required = required_ids if required_ids is not None else set(permission_ids)
    out: list[dict] = []
    for pid in permission_ids:
        meta = get_capability_meta(pid)
        entry: dict = {"id": pid, "required": pid in required}
        if meta is not None:
            entry["label"] = meta["label"]
            entry["description"] = meta["description"]
            entry["category"] = meta["category"]
            entry["risk"] = meta["risk"]
            entry["risk_reason"] = meta["risk_reason"]
        out.append(entry)
    return out


def _unknown_capabilities(permission_ids: list[str]) -> list[str]:
    """Return any ids that are not declared in the catalog."""
    return [pid for pid in permission_ids if not is_known_capability(pid)]


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


@router.get("/v1/plugins/catalog")
async def get_plugin_catalog() -> dict:
    """Return the bundled first-party plugin catalog.

    The webapp's Plugins page reads this to render a marketplace grid
    alongside the file-upload installer. The catalog ships with the
    agent so a fully local-only install still has a browse surface;
    the operator clicks Install and the dashboard hands the
    download_url to ``POST /api/plugins/install_from_url``.

    No I/O outside reading the bundled JSON. Future iterations may
    proxy a remote registry behind a feature flag.
    """
    import json
    from importlib.resources import files

    try:
        raw = (files("ados.data") / "plugin-catalog.json").read_text(
            encoding="utf-8"
        )
        data = json.loads(raw)
    except (FileNotFoundError, OSError, ValueError) as exc:
        return {
            "schema_version": 1,
            "source": "first-party-bundled",
            "plugins": [],
            "error": str(exc),
        }
    return data


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
    permission_ids = sorted(manifest.declared_permissions())
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
            "permissions": _enrich_permissions(permission_ids),
        },
    }


# ---------------------------------------------------------------------
# Parse (non-committing manifest preview)
# ---------------------------------------------------------------------


def _archive_to_summary(raw: bytes) -> dict:
    """Open + signature-verify the archive in memory and return a
    manifest summary suitable for the install dialog. Does NOT touch
    disk and does NOT mutate supervisor state.

    Raises :class:`ManifestError` if any declared permission id is
    not present in the capability catalog so the operator never sees
    a permission row with no label.
    """
    from ados.plugins.archive import parse_archive_bytes
    contents = parse_archive_bytes(raw)
    manifest = contents.manifest
    permission_ids = sorted(manifest.declared_permissions())
    agent_permission_ids = sorted(manifest.declared_agent_permissions())
    unknown = _unknown_capabilities(agent_permission_ids)
    if unknown:
        raise ManifestError(
            "Unknown capability: " + ", ".join(unknown)
        )
    return {
        "ok": True,
        "plugin_id": manifest.id,
        "version": manifest.version,
        "name": manifest.name,
        "description": manifest.description,
        "author": manifest.author,
        "license": manifest.license,
        "risk": manifest.risk,
        "signer_id": contents.signer_id,
        "signed": contents.signature_b64 is not None,
        "halves": (
            ["agent"] if manifest.agent is not None else []
        )
        + (["gcs"] if manifest.gcs is not None else []),
        "permissions": _enrich_permissions(permission_ids),
    }


@router.post("/plugins/parse")
async def parse_plugin_archive(file: UploadFile = File(...)):
    """Validate a ``.adosplug`` archive without committing the install.

    Used by the two-stage install dialog: the GCS uploads, the agent
    parses + signature-checks + returns the manifest summary; the
    operator reviews permissions; the GCS uploads again to ``/install``
    only after consent. Both calls accept the same multipart shape.
    """
    if not file.filename or not file.filename.endswith(".adosplug"):
        return _err(2, "usage_error", "expected a .adosplug file", 400)
    raw = await file.read()
    if not raw:
        return _err(2, "usage_error", "empty upload", 400)
    try:
        return _archive_to_summary(raw)
    except SignatureError as exc:
        return _err(10, f"signature_{exc.kind}", str(exc), 400)
    except (ManifestError, ArchiveError) as exc:
        return _err(12, "manifest_invalid", str(exc), 400)


# ---------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------


@router.post("/plugins/install")
async def install_plugin(
    file: UploadFile = File(...),
    job_id: str | None = None,
    requested_permissions: str | None = None,
):
    """Multipart upload of a ``.adosplug`` archive.

    The archive is read into a temp file (to keep the supervisor's
    on-disk pathing intact), parsed, signature-verified, and the
    manifest summary is returned. Permission grants come on a
    subsequent ``/grant`` call from the install dialog.

    Optional ``job_id`` lets the LAN-direct path write the same
    ``/run/ados/plugin_install_<jobId>.json`` sidecar the cloud-relay
    receiver writes, so the WebSocket progress route serves both
    transports the same way. Optional comma-separated
    ``requested_permissions`` triggers immediate grants on the freshly
    installed plugin — used by the install dialog so the operator does
    not have to click through a separate grant flow.
    """
    if not file.filename or not file.filename.endswith(".adosplug"):
        return _err(2, "usage_error", "expected a .adosplug file", 400)
    raw = await file.read()
    if not raw:
        return _err(2, "usage_error", "empty upload", 400)
    sup = _get_supervisor()
    if job_id:
        write_sidecar(job_id, {"stage": "verifying"})
    # Pre-flight catalog check: refuse archives whose manifest
    # declares a capability id we cannot label in the install dialog.
    # This runs before the supervisor commits so a rejected install
    # leaves no on-disk residue.
    try:
        from ados.plugins.archive import parse_archive_bytes
        preview = parse_archive_bytes(raw)
        unknown = _unknown_capabilities(
            sorted(preview.manifest.declared_agent_permissions())
        )
        if unknown:
            return _err(
                12,
                "manifest_invalid",
                "Unknown capability: " + ", ".join(unknown),
                400,
            )
    except SignatureError as exc:
        return _err(10, f"signature_{exc.kind}", str(exc), 400)
    except ManifestError as exc:
        return _err(12, "manifest_invalid", str(exc), 400)
    except ArchiveError as exc:
        return _err(12, "archive_invalid", str(exc), 400)
    with tempfile.NamedTemporaryFile(
        suffix=".adosplug", delete=False
    ) as tmp:
        tmp.write(raw)
        tmp_path = Path(tmp.name)
    try:
        if job_id:
            write_sidecar(job_id, {"stage": "installing"})
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

    granted_ids: list[str] = []
    if requested_permissions:
        wanted = [p.strip() for p in requested_permissions.split(",") if p.strip()]
        for perm in wanted:
            try:
                sup.grant_permission(result.plugin_id, perm)
                granted_ids.append(perm)
            except SupervisorError as exc:
                log.warning(
                    "plugin_install_grant_skip",
                    permission=perm,
                    error=str(exc),
                )

    if job_id:
        write_sidecar(
            job_id,
            {"stage": "completed", "pluginId": result.plugin_id},
        )

    return {
        "ok": True,
        "plugin_id": result.plugin_id,
        "version": result.version,
        "signer_id": result.signer_id,
        "risk": result.risk,
        "permissions_requested": result.permissions_requested,
        "granted": granted_ids,
        "job_id": job_id,
    }


# ---------------------------------------------------------------------
# Install from URL
# ---------------------------------------------------------------------


class InstallFromUrlRequest(BaseModel):
    """Body of ``POST /api/plugins/install_from_url``.

    The GCS sends the canonical published archive URL (typically a
    GitHub release asset for the extensions repo) along with a SHA-256
    pin that the registry seeder publishes. The optional
    ``requested_permissions`` list mirrors the multipart endpoint and
    triggers immediate grants on the freshly installed plugin so the
    install dialog does not have to make a separate ``/grant`` call.

    ``from_catalog`` flips when the install was triggered by the
    first-party catalog browser on the agent webapp. Catalog entries
    must pin a SHA — the route rejects them when the pin is absent.
    """

    url: str
    expected_sha256: str | None = None
    requested_permissions: list[str] | None = None
    job_id: str | None = None
    from_catalog: bool = False


@router.post("/plugins/install_from_url")
async def install_plugin_from_url(body: InstallFromUrlRequest):
    """Download a ``.adosplug`` archive from an allowlisted URL and install it.

    Companion to the multipart ``/install`` endpoint. Used by the GCS
    Plugins page when the plugin is a registry entry whose canonical
    binary is already hosted at a public URL — no intermediate storage
    hop is needed. The download is streamed with a hard size cap and
    a SHA-256 pin (required when ``from_catalog=true`` so first-party
    catalog installs never skip integrity verification, optional
    otherwise so operator-supplied URLs without a pin still work);
    everything past the bytes-on-disk handoff reuses the same
    supervisor flow as the multipart endpoint.
    """
    url = (body.url or "").strip()
    if not url:
        return _err(2, "usage_error", "url required", 400)

    try:
        validate_install_url(url)
    except UrlValidationError as exc:
        return _err(2, "url_invalid", str(exc), 400)

    expected_sha = (body.expected_sha256 or "").strip()
    if body.from_catalog and not expected_sha:
        return _err(
            2,
            "sha256_required",
            "catalog installs must pin archive_sha256",
            400,
        )
    requested = list(body.requested_permissions or [])
    job_id = body.job_id

    sup = _get_supervisor()
    if job_id:
        write_sidecar(job_id, {"stage": "downloading"})

    timeout = httpx.Timeout(
        DOWNLOAD_TOTAL_TIMEOUT,
        connect=DOWNLOAD_CONNECT_TIMEOUT,
    )
    # Per-request fresh tempdir. On a deployed agent the systemd unit
    # ships with ProtectSystem=strict and the only writable paths are
    # /var/ados, /run/ados, /etc/ados — /tmp and /var/tmp are sealed.
    # Root the tempdir under /run/ados/ so the streamed archive lands on
    # a path the sandbox allows. In dev or unit tests /run/ados/ may not
    # exist; fall back to the system tempdir there.
    tempdir_kwargs: dict[str, str] = {"prefix": "ados-plug-url-"}
    plugin_tmp_root = Path("/run/ados/plugin-downloads")
    if plugin_tmp_root.parent.exists():
        plugin_tmp_root.mkdir(parents=True, exist_ok=True)
        tempdir_kwargs["dir"] = str(plugin_tmp_root)
    with tempfile.TemporaryDirectory(**tempdir_kwargs) as tmp_dir:
        archive_path = Path(tmp_dir) / "archive.adosplug"
        try:
            async with httpx.AsyncClient(timeout=timeout) as client:
                outcome = await stream_archive_to_path(
                    client=client,
                    url=url,
                    dest=archive_path,
                    expected_sha256=expected_sha,
                )
        except ArchiveTooLargeError as exc:
            if job_id:
                write_sidecar(job_id, {"stage": "failed", "detail": str(exc)})
            return _err(13, "archive_too_large", str(exc), 413)
        except Sha256MismatchError as exc:
            # Log the computed digest at DEBUG (operator can pull it
            # from the journal with full agent log access) but DO NOT
            # echo it back on the wire. A pinned mismatch tells the
            # caller their pin is wrong; the actual hash of whatever
            # the URL served is an oracle they should not get from
            # the API surface.
            log.debug("plugin sha256 mismatch", detail=str(exc))
            if job_id:
                write_sidecar(
                    job_id,
                    {"stage": "failed", "detail": "archive sha256 did not match pin"},
                )
            return _err(
                12,
                "sha256_mismatch",
                "archive sha256 did not match pin",
                400,
            )
        except ArchiveDownloadError as exc:
            if job_id:
                write_sidecar(job_id, {"stage": "failed", "detail": str(exc)})
            return _err(20, "download_failed", str(exc), 502)
        except httpx.HTTPError as exc:
            if job_id:
                write_sidecar(job_id, {"stage": "failed", "detail": str(exc)})
            return _err(20, "download_failed", str(exc), 502)

        if job_id:
            write_sidecar(job_id, {"stage": "verifying"})

        # Reuse the same unknown-capability preflight the multipart
        # path runs so the on-disk install attempt is skipped when the
        # manifest declares a capability the host does not recognise.
        try:
            from ados.plugins.archive import parse_archive_bytes
            raw = archive_path.read_bytes()
            preview = parse_archive_bytes(raw)
            unknown = _unknown_capabilities(
                sorted(preview.manifest.declared_agent_permissions())
            )
            if unknown:
                if job_id:
                    write_sidecar(
                        job_id,
                        {
                            "stage": "failed",
                            "detail": "unknown capability",
                        },
                    )
                return _err(
                    12,
                    "manifest_invalid",
                    "Unknown capability: " + ", ".join(unknown),
                    400,
                )
        except SignatureError as exc:
            return _err(10, f"signature_{exc.kind}", str(exc), 400)
        except ManifestError as exc:
            return _err(12, "manifest_invalid", str(exc), 400)
        except ArchiveError as exc:
            return _err(12, "archive_invalid", str(exc), 400)

        if job_id:
            write_sidecar(job_id, {"stage": "installing"})
        try:
            result = sup.install_archive(archive_path)
        except SignatureError as exc:
            return _err(
                10,
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

    granted_ids: list[str] = []
    for perm in requested:
        perm = (perm or "").strip()
        if not perm:
            continue
        try:
            sup.grant_permission(result.plugin_id, perm)
            granted_ids.append(perm)
        except SupervisorError as exc:
            log.warning(
                "plugin_install_from_url_grant_skip",
                permission=perm,
                error=str(exc),
            )

    if job_id:
        write_sidecar(
            job_id,
            {"stage": "completed", "pluginId": result.plugin_id},
        )

    log.info(
        "plugin_install_from_url_ok",
        plugin_id=result.plugin_id,
        version=result.version,
        sha256=outcome.sha256_hex,
        bytes=outcome.byte_count,
    )

    return {
        "ok": True,
        "plugin_id": result.plugin_id,
        "version": result.version,
        "signer_id": result.signer_id,
        "risk": result.risk,
        "permissions_requested": result.permissions_requested,
        "granted": granted_ids,
        "job_id": job_id,
        "sha256": outcome.sha256_hex,
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


@router.delete("/plugins/{plugin_id}/perms/{permission_id}")
async def revoke_plugin_permission(plugin_id: str, permission_id: str):
    sup = _get_supervisor()
    try:
        sup.revoke_permission(plugin_id, permission_id)
    except SupervisorError as exc:
        if "not installed" in str(exc):
            return _err(14, "not_found", str(exc), 404)
        return _err(20, "host_io_error", str(exc), 500)
    install = sup.find_install(plugin_id)
    granted = (
        sorted(p for p, g in install.permissions.items() if g.granted)
        if install is not None
        else []
    )
    return {
        "ok": True,
        "plugin_id": plugin_id,
        "granted": granted,
        "requires_restart": False,
    }


# ---------------------------------------------------------------------
# One-shot WebSocket ticket mint
# ---------------------------------------------------------------------


@router.post("/plugins/jobs/{job_id}/ticket")
async def mint_install_job_ticket(job_id: str) -> dict:
    """Issue a one-shot ticket the GCS uses to open the progress WS.

    Browsers cannot set ``X-ADOS-Key`` on a WebSocket handshake, so
    the previous design fell back to ``?api_key=<pairing_key>`` in
    the URL — which leaks into DevTools, HAR exports, and any
    reverse-proxy access log. This route lets the GCS exchange its
    pairing key (enforced on the REST middleware) for a short-lived
    random ticket and hand the ticket to ``new WebSocket(url,
    ["ados-job-ticket", ticket])``. The agent validates and consumes
    the ticket on the WebSocket handshake.

    Ticket lifetime: 30 s. One-shot: the second connect with the
    same ticket fails.
    """
    if not job_id:
        return _err(2, "usage_error", "job_id required", 400)
    ticket, expires_at = await job_ticket_store.issue(job_id)
    return {"ok": True, "ticket": ticket, "expiresAt": expires_at}


# ---------------------------------------------------------------------
# WebSocket: in-flight install job progress
# ---------------------------------------------------------------------


@router.websocket("/plugins/jobs/{job_id}")
async def stream_install_job(websocket: WebSocket, job_id: str) -> None:
    """Stream install-job progress from the in-flight sidecar JSON.

    Both transports write the same sidecar so this route is
    transport-agnostic. Closes on terminal stage or idle timeout.

    The Starlette HTTP middleware does not process WebSocket
    handshakes, so the paired-key check runs inline before
    ``accept()``. Native clients can pass ``X-ADOS-Key``; browsers
    pass a one-shot ticket via the ``ados-job-ticket`` subprotocol.
    The previous ``?api_key=`` query-string fallback is gone — the
    URL must not carry the pairing key.
    """
    accept_subprotocol = await authenticate_job_websocket(
        websocket, job_id=job_id
    )
    if accept_subprotocol is None:
        return
    if accept_subprotocol:
        await websocket.accept(subprotocol=accept_subprotocol)
    else:
        await websocket.accept()
    try:
        await run_job_progress_stream(websocket, job_id)
    except WebSocketDisconnect:
        return
    except Exception as exc:  # pragma: no cover — defensive
        log.warning("plugin_job_ws_error", job_id=job_id, error=str(exc))


# ---------------------------------------------------------------------
# Capability-token mint (agent issuer)
# ---------------------------------------------------------------------


class CapabilityTokenRequest(BaseModel):
    plugin_id: str
    operator_id: str | None = None
    ttl_seconds: int | None = None


@router.post("/plugins/capability-token")
async def mint_capability_token(body: CapabilityTokenRequest):
    """Mint an ``iss: agent:<device_id>`` capability token.

    The token rides postMessage RPCs from the plugin iframe to the GCS
    bridge, which verifies the same HMAC the agent signs with (HKDF
    from the pairing key + spec'd salt).
    """
    app = get_agent_app()
    pairing_key = getattr(app.pairing_manager, "api_key", None)
    if not pairing_key:
        return _err(
            11,
            "not_paired",
            "agent must be paired to mint capability tokens",
            409,
        )

    sup = _get_supervisor()
    install = sup.find_install(body.plugin_id)
    if install is None:
        return _err(14, "not_found", f"plugin {body.plugin_id} not installed", 404)

    granted, audit = compute_granted_caps_for_token(
        plugin_id=body.plugin_id,
        in_memory_permissions=install.permissions,
    )

    operator_id = (
        body.operator_id or (audit or {}).get("operator_id") or "unknown"
    )
    token, claims = mint_agent_capability_token(
        plugin_id=body.plugin_id,
        agent_id=app.config.agent.device_id,
        operator_id=str(operator_id),
        granted_capabilities=granted,
        pairing_key=pairing_key,
        ttl_seconds=body.ttl_seconds or TOKEN_TTL_SECONDS_DEFAULT,
    )
    return {
        "ok": True,
        "token": token,
        "expiresAt": claims.expires_at_ms,
        "issuer": claims.issuer,
        "grantedCapabilities": list(claims.granted_capabilities),
    }
