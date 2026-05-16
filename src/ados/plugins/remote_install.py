"""Cloud-relay install receiver — fallback path for the plugin install flow.

The local-first multipart upload to ``POST /api/plugins/install`` is the
primary transport. This module handles the case where the GCS cannot
reach the agent directly (4G, NAT, HTTPS site) and the install rides
the Convex command queue instead. Both transports converge on the
same :meth:`PluginSupervisor.install_archive` call; we do not
re-implement signature verify, unpack, or systemd-unit render.

Owns: idempotency ring, retried signed-URL download with one-shot
refresh, sidecar JSON stage reporting, granted-permissions audit log,
and the ``cmd_droneCommands`` ack shape.
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import time
from pathlib import Path
from typing import Any

import httpx

from ados.api.routes._plugins_helpers import (
    clear_sidecar,
    is_terminal_stage,
    write_granted_permissions_yaml,
    write_sidecar,
)
from ados.core.logging import get_logger
from ados.plugins.errors import (
    ArchiveError,
    ManifestError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.remote_install_download import (
    CONVEX_HOST_SUFFIXES,
    DOWNLOAD_MAX_BYTES,
    DownloadError,
    stream_download,
    validate_download_url,
    verify_sha256,
)
from ados.plugins.supervisor import PluginSupervisor

log = get_logger("plugins.remote_install")


# Idempotency ring lives next to the install state under /var/lib so it
# survives reboots. mtime-rotated weekly so the file does not grow
# unbounded across a long agent lifetime.
SEEN_JOBS_DIR = Path("/var/lib/ados/plugins/.jobs")
SEEN_JOBS_PATH = SEEN_JOBS_DIR / "_seen_jobs.json"
SEEN_JOBS_MAX = 10_000
SEEN_JOBS_ROTATE_SECONDS = 7 * 24 * 3600

# Download retry ladder. 1 s, 4 s, 16 s. Spec'd by the plan.
DOWNLOAD_RETRY_DELAYS = (1.0, 4.0, 16.0)
DOWNLOAD_TIMEOUT_SECONDS = 60.0

# Sidecar stages reflected on heartbeat. Matches the GCS install dialog
# six-stage progress UI: queued → downloading → verifying → installing
# → enabling → completed.
STAGE_QUEUED = "queued"
STAGE_DOWNLOADING = "downloading"
STAGE_VERIFYING = "verifying"
STAGE_INSTALLING = "installing"
STAGE_ENABLING = "enabling"
STAGE_COMPLETED = "completed"
STAGE_FAILED = "failed"


# ---------------------------------------------------------------------
# Idempotency ring
# ---------------------------------------------------------------------


def _load_seen_jobs(path: Path = SEEN_JOBS_PATH) -> dict[str, int]:
    """Load + rotate the seen-jobs map. Returns ``{jobId: ts_ms}``."""
    if not path.exists():
        return {}
    try:
        # Rotate when the file is older than a week to bound disk use.
        if time.time() - path.stat().st_mtime > SEEN_JOBS_ROTATE_SECONDS:
            return {}
        raw = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(raw, dict):
            return {}
        # Defensive: drop entries with non-int values
        return {k: int(v) for k, v in raw.items() if isinstance(v, (int, float))}
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        log.warning("seen_jobs_load_failed", error=str(exc))
        return {}


def _save_seen_jobs(seen: dict[str, int], path: Path = SEEN_JOBS_PATH) -> None:
    """Atomic write of the seen-jobs map with a size cap.

    When over cap, drop the oldest 10% of entries to keep churn low.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    if len(seen) > SEEN_JOBS_MAX:
        # Sort by ts ascending; drop the oldest ~10%.
        ordered = sorted(seen.items(), key=lambda kv: kv[1])
        keep = ordered[len(ordered) // 10 :]
        seen = dict(keep)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=str(path.parent),
        delete=False,
    ) as tmp:
        json.dump(seen, tmp, sort_keys=True)
        tmp_path = Path(tmp.name)
    os.replace(tmp_path, path)


def already_seen(job_id: str, *, path: Path = SEEN_JOBS_PATH) -> bool:
    return job_id in _load_seen_jobs(path)


def mark_seen(job_id: str, *, path: Path = SEEN_JOBS_PATH) -> None:
    seen = _load_seen_jobs(path)
    seen[job_id] = int(time.time() * 1000)
    _save_seen_jobs(seen, path)


# ---------------------------------------------------------------------
# Stage reporting
# ---------------------------------------------------------------------


def _emit_stage(
    job_id: str,
    stage: str,
    *,
    plugin_id: str | None,
    detail: str | None = None,
    sidecar_root: Path | None = None,
) -> None:
    """Write the sidecar JSON. Stage is also reflected on the heartbeat
    by the cloud loop reading the same file.
    """
    payload: dict[str, Any] = {"stage": stage}
    if plugin_id is not None:
        payload["pluginId"] = plugin_id
    if detail is not None:
        payload["detail"] = detail
    try:
        write_sidecar(job_id, payload, root=sidecar_root)
    except OSError as exc:
        log.warning(
            "stage_sidecar_write_failed",
            job_id=job_id,
            stage=stage,
            error=str(exc),
        )


# ---------------------------------------------------------------------
# Error classification
# ---------------------------------------------------------------------


def _classify_install_error(exc: Exception) -> tuple[str, str]:
    """Map a supervisor exception into ``(code, kind_msg)``."""
    if isinstance(exc, SignatureError):
        return f"signature_{exc.kind}", "signature"
    if isinstance(exc, (ManifestError, ArchiveError)):
        return "manifest_invalid", "manifest"
    return "supervisor_error", "install"


# ---------------------------------------------------------------------
# RemoteInstallReceiver
# ---------------------------------------------------------------------


class RemoteInstallReceiver:
    """Bridges cloud-relay commands to the local supervisor pipeline.

    All entry points are coroutines so the cloud poll loop can await
    them without blocking the heartbeat. The supervisor itself is
    synchronous; long-running work runs under ``asyncio.to_thread`` to
    keep the event loop responsive.
    """

    @classmethod
    async def handle_install(
        cls,
        cmd: dict,
        *,
        supervisor: PluginSupervisor,
        device_id: str,
        api_key: str | None = None,
        convex_url: str | None = None,
        sidecar_root: Path | None = None,
        seen_jobs_path: Path = SEEN_JOBS_PATH,
        grants_root: Path | None = None,
        http_client: httpx.AsyncClient | None = None,
    ) -> tuple[str, dict | None, dict | None]:
        """Run the cloud-relay install for ``plugin.install`` commands."""
        args = cmd.get("args") or {}
        job_id = str(args.get("jobId") or cmd.get("_id") or "")
        plugin_id = args.get("pluginId")
        operator_id = args.get("operatorId") or args.get("userId") or "unknown"
        archive_id = args.get("archiveId") or ""
        signed_url = args.get("signedUrl") or ""
        requested = list(args.get("requestedPermissions") or [])
        # Optional defense-in-depth: when the command queue declares
        # the manifest hash up front we verify the downloaded bytes
        # against it before handing the archive to the supervisor.
        expected_sha = args.get("manifestHash") or args.get("archiveSha256") or ""

        if not job_id:
            return "failed", {"success": False, "message": "jobId required"}, None

        # Idempotency: replayed command rows are no-ops.
        if already_seen(job_id, path=seen_jobs_path):
            log.info("remote_install_replay_skip", job_id=job_id)
            return "completed", {"success": True, "message": "already_processed"}, {
                "jobId": job_id,
                "replay": True,
            }

        _emit_stage(
            job_id, STAGE_QUEUED, plugin_id=plugin_id, sidecar_root=sidecar_root
        )

        owns_client = http_client is None
        client = http_client or httpx.AsyncClient(timeout=DOWNLOAD_TIMEOUT_SECONDS)
        try:
            try:
                archive_bytes = await cls._download_with_refresh(
                    client=client,
                    job_id=job_id,
                    plugin_id=plugin_id,
                    signed_url=signed_url,
                    archive_id=archive_id,
                    convex_url=convex_url,
                    api_key=api_key,
                    sidecar_root=sidecar_root,
                    expected_sha256=expected_sha,
                )
            except Exception as exc:  # noqa: BLE001
                _emit_stage(job_id, STAGE_FAILED, plugin_id=plugin_id, detail=f"download: {exc}", sidecar_root=sidecar_root)
                return "failed", {"success": False, "message": f"download failed: {exc}"}, {"code": "download_failed", "jobId": job_id}

            # Stage the archive on disk so the supervisor's existing
            # entry point — which expects a Path — keeps working.
            with tempfile.NamedTemporaryFile(
                suffix=".adosplug", delete=False
            ) as tmp:
                tmp.write(archive_bytes)
                tmp_path = Path(tmp.name)

            try:
                _emit_stage(job_id, STAGE_VERIFYING, plugin_id=plugin_id, sidecar_root=sidecar_root)
                _emit_stage(job_id, STAGE_INSTALLING, plugin_id=plugin_id, sidecar_root=sidecar_root)
                result = await asyncio.to_thread(supervisor.install_archive, tmp_path)
            except (SignatureError, ManifestError, ArchiveError, SupervisorError) as exc:
                code, kind_msg = _classify_install_error(exc)
                msg = f"{kind_msg}: {exc}"
                _emit_stage(job_id, STAGE_FAILED, plugin_id=plugin_id, detail=msg, sidecar_root=sidecar_root)
                return "failed", {"success": False, "message": msg}, {"code": code, "jobId": job_id}
            finally:
                try:
                    tmp_path.unlink(missing_ok=True)
                except OSError as exc:
                    log.warning("remote_install_tmp_cleanup_failed", job_id=job_id, error=str(exc))

            # Apply requested permission grants. Supervisor knows how
            # to filter against the manifest. Skip silently on errors;
            # the install succeeded and the operator can re-grant later.
            granted_ids: list[str] = []
            for perm_id in requested:
                try:
                    await asyncio.to_thread(supervisor.grant_permission, result.plugin_id, perm_id)
                    granted_ids.append(perm_id)
                except SupervisorError as exc:
                    log.warning("remote_install_grant_skip", job_id=job_id, permission=perm_id, error=str(exc))

            # Persist the audit record. Per-(operator, drone, plugin).
            try:
                write_granted_permissions_yaml(
                    plugin_id=result.plugin_id,
                    operator_id=operator_id,
                    agent_id=device_id,
                    granted=granted_ids,
                    root=grants_root,
                )
            except OSError as exc:
                log.warning("remote_install_grants_write_failed", job_id=job_id, error=str(exc))

            _emit_stage(job_id, STAGE_ENABLING, plugin_id=result.plugin_id, sidecar_root=sidecar_root)
            _emit_stage(job_id, STAGE_COMPLETED, plugin_id=result.plugin_id, sidecar_root=sidecar_root)
            mark_seen(job_id, path=seen_jobs_path)

            install_record = supervisor.find_install(result.plugin_id)
            manifest_hash = install_record.manifest_hash if install_record is not None else ""
            return "completed", {"success": True, "message": "installed"}, {
                "installId": job_id,
                "pluginId": result.plugin_id,
                "version": result.version,
                "signerId": result.signer_id,
                "manifestHash": manifest_hash,
            }
        finally:
            if owns_client:
                await client.aclose()

    @classmethod
    async def dispatch(
        cls,
        cmd: dict,
        *,
        supervisor: PluginSupervisor,
        device_id: str,
        sidecar_root: Path | None = None,
        seen_jobs_path: Path = SEEN_JOBS_PATH,
    ) -> tuple[str, dict | None, dict | None]:
        """Handle non-install lifecycle commands.

        Covers ``plugin.uninstall``, ``plugin.enable``, ``plugin.disable``,
        and ``plugin.configure``. ``plugin.install`` is routed through
        :meth:`handle_install` because it needs the download path.
        """
        command = cmd.get("command", "")
        args = cmd.get("args") or {}
        plugin_id = args.get("pluginId") or ""
        job_id = str(args.get("jobId") or cmd.get("_id") or plugin_id or "")

        if not plugin_id:
            return "failed", {"success": False, "message": "pluginId required"}, None

        if already_seen(job_id, path=seen_jobs_path):
            return "completed", {"success": True, "message": "already_processed"}, {
                "jobId": job_id,
                "replay": True,
            }

        try:
            if command == "plugin.uninstall":
                keep_data = bool(args.get("keepData", False))
                await asyncio.to_thread(
                    supervisor.remove, plugin_id, keep_data=keep_data
                )
                action = "uninstalled"
            elif command == "plugin.enable":
                await asyncio.to_thread(supervisor.enable, plugin_id)
                action = "enabled"
            elif command == "plugin.disable":
                await asyncio.to_thread(supervisor.disable, plugin_id)
                action = "disabled"
            elif command == "plugin.configure":
                # Configure is a permission-grant batch in the v1 wire shape.
                for perm in list(args.get("grantPermissions") or []):
                    await asyncio.to_thread(supervisor.grant_permission, plugin_id, perm)
                for perm in list(args.get("revokePermissions") or []):
                    await asyncio.to_thread(supervisor.revoke_permission, plugin_id, perm)
                action = "configured"
            else:
                return "failed", {"success": False, "message": f"unknown plugin command {command}"}, None
        except SupervisorError as exc:
            return "failed", {"success": False, "message": str(exc)}, {"code": "supervisor_error", "jobId": job_id}

        mark_seen(job_id, path=seen_jobs_path)
        return "completed", {"success": True, "message": action}, {"jobId": job_id, "pluginId": plugin_id, "action": action}

    # -----------------------------------------------------------------
    # Internal: download with retry + signed-URL refresh
    # -----------------------------------------------------------------

    @classmethod
    async def _download_with_refresh(
        cls,
        *,
        client: httpx.AsyncClient,
        job_id: str,
        plugin_id: str | None,
        signed_url: str,
        archive_id: str,
        convex_url: str | None,
        api_key: str | None,
        sidecar_root: Path | None,
        expected_sha256: str = "",
    ) -> bytes:
        """Fetch the archive. Refresh the signed URL once on 401.

        Defenses applied here (in order):

        1. Reject any URL whose scheme is not ``https`` or whose host
           is not on :data:`CONVEX_HOST_SUFFIXES`. A compromised cloud
           command row cannot redirect to attacker-controlled hosts.
        2. Stream the body with a hard byte cap so a malicious 10 GB
           response cannot OOM the agent before the archive layer
           sees it.
        3. When the command queue declares a ``manifestHash``, verify
           the downloaded bytes against it. Defense-in-depth on top
           of the Ed25519 signature check that supervisor runs later.
        """
        if not signed_url:
            raise DownloadError("signedUrl missing from command args")
        validate_download_url(signed_url)
        _emit_stage(job_id, STAGE_DOWNLOADING, plugin_id=plugin_id, sidecar_root=sidecar_root)

        refreshed = False
        current_url = signed_url
        last_exc: Exception | None = None
        for delay in (0.0, *DOWNLOAD_RETRY_DELAYS):
            if delay > 0:
                await asyncio.sleep(delay)
            try:
                status_code, body = await stream_download(client, current_url)
            except DownloadError:
                # Hard rejections (scheme/host/size) should NOT retry;
                # the bytes are not getting any better next attempt.
                raise
            except httpx.HTTPError as exc:
                last_exc = exc
                continue

            if status_code == 200:
                verify_sha256(body, expected_sha256)
                return body
            if status_code == 401 and not refreshed and convex_url:
                refreshed = True
                current_url = await cls._refresh_signed_url(
                    client=client, convex_url=convex_url, api_key=api_key,
                    job_id=job_id, archive_id=archive_id,
                )
                validate_download_url(current_url)
                continue
            last_exc = RuntimeError(f"download HTTP {status_code}")
        raise last_exc or RuntimeError("download exhausted retries")

    @classmethod
    async def _refresh_signed_url(
        cls,
        *,
        client: httpx.AsyncClient,
        convex_url: str,
        api_key: str | None,
        job_id: str,
        archive_id: str,
    ) -> str:
        """Call Convex HTTP action ``refreshDownload`` and return a new URL."""
        url = f"{convex_url.rstrip('/')}/agent/plugins/refreshDownload"
        headers = {"X-ADOS-Key": api_key} if api_key else {}
        resp = await client.post(
            url,
            json={"jobId": job_id, "archiveId": archive_id},
            headers=headers,
        )
        if resp.status_code != 200:
            raise RuntimeError(
                f"refreshDownload HTTP {resp.status_code}"
            )
        body = resp.json()
        new_url = body.get("signedUrl") or body.get("url")
        if not new_url:
            raise RuntimeError("refreshDownload returned no signedUrl")
        return str(new_url)


def is_plugin_command(command: str) -> bool:
    """Return True when ``command`` belongs on this receiver."""
    return command in {
        "plugin.install",
        "plugin.uninstall",
        "plugin.enable",
        "plugin.disable",
        "plugin.configure",
    }


__all__ = [
    "CONVEX_HOST_SUFFIXES",
    "DOWNLOAD_MAX_BYTES",
    "DownloadError",
    "RemoteInstallReceiver",
    "already_seen",
    "clear_sidecar",
    "is_plugin_command",
    "is_terminal_stage",
    "mark_seen",
]
