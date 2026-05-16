"""Daily auto-update poll for installed third-party plugins.

Once per day (with jitter to prevent fleet-wide thundering herd) the
cloud service iterates every enabled install record and asks the
registry for the latest published version. The decision tree:

    silent install     ⇐ patch or minor bump AND permissions
                          unchanged AND board still supported AND
                          plugin is not pinned AND auto_update is on.
                          Signatures verify inside the supervisor.
    notify-only        ⇐ major bump, permission delta, or
                          board mismatch. Published over MQTT so
                          the GCS can surface a toast and a button
                          taking the operator to the install
                          dialog.
    skipped            ⇐ pinned, auto_update off, or no newer
                          version published.
    failed             ⇐ network / signature / install error.
                          The failure is recorded on the install
                          record so the GCS shows a banner. The
                          next poll re-tries.

The agent never re-verifies signatures itself: the supervisor's
``install_archive`` entry point owns that contract, raises
``SignatureError`` on mismatch, and refuses to mutate state when the
verify fails. This module catches the four documented exception
types from ``ados.plugins.errors`` and records them on the install.

Convex query transport: ``POST {convex_url}/api/query`` with the
standard JSON body
``{"path": "pluginRegistry:getPlugin", "args": {"pluginId": "..."},
"format": "json"}``. Authenticated via ``X-ADOS-Key`` header. The
registry stores the manifest YAML on each version row; permissions
and supported-boards lists are extracted from that blob.

The download path reuses ``remote_install_download``: same hostname
allowlist, same streaming size cap, same SHA-256 verify hook.
"""

from __future__ import annotations

import asyncio
import json
import random
import tempfile
import time
from enum import Enum
from pathlib import Path
from typing import Any

import httpx

from ados.core.logging import get_logger
from ados.plugins.errors import (
    ArchiveError,
    ManifestError,
    SignatureError,
    SupervisorError,
)
from ados.plugins.manifest import PluginManifest
from ados.plugins.remote_install_download import (
    DownloadError,
    stream_download,
    validate_download_url,
    verify_sha256,
)
from ados.plugins.state import (
    PluginInstall,
    PermissionGrant,
    load_state,
    save_state,
    state_lock,
)
from ados.plugins.supervisor import PluginSupervisor

log = get_logger("plugins.auto_update")


# Daily cadence with +/- one hour of jitter so a fleet of devices on
# the same install timestamp does not stampede the registry every
# 24 hours. The jitter range is hard-capped so a misconfigured clock
# cannot push the next poll into next week.
DAILY_SLEEP_SECONDS = 24 * 3600
JITTER_RANGE_SECONDS = 3600

# Default HTTP timeout for registry calls. Same posture as the
# heartbeat loop's 10 s budget.
REGISTRY_TIMEOUT_SECONDS = 30.0


class AutoUpdateOutcome(str, Enum):
    """Result of evaluating one plugin against the registry."""

    SILENT_INSTALL = "silent_install"
    NOTIFY = "notify"
    SKIPPED = "skipped"
    FAILED = "failed"


def _now_ms() -> int:
    return int(time.time() * 1000)


# ---------------------------------------------------------------------
# Semver compare (tiny inline helper — packaging lib is NOT a dep)
# ---------------------------------------------------------------------


def _parse_semver(version: str) -> tuple[int, int, int]:
    """Return ``(major, minor, patch)`` from a semver string.

    Strips any ``-prerelease`` / ``+build`` suffix the same way the
    supervisor's ``_semver_tuple`` does so the two helpers stay in
    sync. Raises ``ValueError`` on unparseable input — the caller
    treats that as "skip this plugin, log a warning".
    """
    base = version.split("-", 1)[0].split("+", 1)[0]
    parts = base.split(".")
    if len(parts) < 3:
        parts = (parts + ["0", "0", "0"])[:3]
    return (int(parts[0]), int(parts[1]), int(parts[2]))


def _is_newer(latest: str, current: str) -> bool:
    """True if ``latest`` is strictly greater than ``current``."""
    try:
        return _parse_semver(latest) > _parse_semver(current)
    except (ValueError, IndexError):
        return False


def _is_major_bump(latest: str, current: str) -> bool:
    """True if ``latest.major > current.major``."""
    try:
        return _parse_semver(latest)[0] > _parse_semver(current)[0]
    except (ValueError, IndexError):
        # When we can't parse, treat as the safer-to-notify path.
        return True


# ---------------------------------------------------------------------
# Registry RPC
# ---------------------------------------------------------------------


async def _registry_get_plugin(
    client: httpx.AsyncClient,
    convex_url: str,
    api_key: str | None,
    plugin_id: str,
) -> dict | None:
    """Call ``pluginRegistry.getPlugin`` and return its raw payload.

    Convex public queries are reachable over HTTP at
    ``POST /api/query`` with a ``{path, args, format}`` body. The
    response envelope is ``{"status": "success", "value": <result>}``
    on a successful query. Returns ``None`` when the registry has no
    row for that plugin id (Convex returns ``"value": null``).
    """
    url = f"{convex_url.rstrip('/')}/api/query"
    headers = {"X-ADOS-Key": api_key} if api_key else {}
    body = {
        "path": "pluginRegistry:getPlugin",
        "args": {"pluginId": plugin_id},
        "format": "json",
    }
    resp = await client.post(url, json=body, headers=headers)
    if resp.status_code != 200:
        raise RuntimeError(
            f"registry getPlugin HTTP {resp.status_code}: {resp.text[:200]}"
        )
    envelope = resp.json()
    if envelope.get("status") != "success":
        raise RuntimeError(
            f"registry getPlugin error: {envelope.get('errorMessage', 'unknown')}"
        )
    return envelope.get("value")


def _pick_latest_version_row(payload: dict) -> dict | None:
    """Extract the most recent version row from the ``getPlugin`` payload.

    The Convex query orders versions by ``released_at`` descending and
    returns up to 20. We just take the first.
    """
    versions = payload.get("versions") or []
    if not versions:
        return None
    return versions[0]


def _parse_version_manifest(version_row: dict) -> PluginManifest | None:
    """Parse the manifest YAML embedded in a registry version row.

    Returns ``None`` when the YAML is missing or fails to parse — the
    caller treats that as "skip, log".
    """
    yaml_text = version_row.get("manifest_yaml") or ""
    if not yaml_text:
        return None
    try:
        return PluginManifest.from_yaml_text(yaml_text)
    except ManifestError as exc:
        log.warning(
            "auto_update_remote_manifest_unparseable", error=str(exc)
        )
        return None


# ---------------------------------------------------------------------
# MQTT notify
# ---------------------------------------------------------------------


def _publish_update_notice(
    device_id: str,
    payload: dict,
) -> None:
    """Best-effort MQTT publish of an ``update_available`` event.

    The cloud service holds the live MQTT client behind the
    :class:`MqttGateway` instance. We hand the topic + JSON payload
    to a tiny helper imported at call time so this module does not
    own a long-lived client of its own (the gateway already runs as
    its own task in the cloud service).

    QoS 1 because we want the GCS to receive the notice even across
    a brief broker reconnect. Topic shape mirrors the rest of the
    agent's MQTT surface: ``ados/{device_id}/plugin/update_available``.
    """
    try:
        import paho.mqtt.publish as _publish  # type: ignore[import-untyped]
    except ImportError:
        log.debug("auto_update_mqtt_unavailable")
        return
    topic = f"ados/{device_id}/plugin/update_available"
    # We deliberately do NOT block on the publish here; the cloud
    # heartbeat already surfaces ``lastPluginUpdateCheckAt`` so the
    # GCS has a fallback path. A short connect + publish loop is
    # the simplest portable shape that doesn't need the gateway's
    # connection state.
    try:
        _publish.single(
            topic,
            payload=json.dumps(payload),
            qos=1,
            hostname="127.0.0.1",
            port=1883,
            keepalive=10,
        )
    except Exception as exc:  # noqa: BLE001
        log.debug("auto_update_mqtt_publish_failed", error=str(exc))


# ---------------------------------------------------------------------
# Decision tree
# ---------------------------------------------------------------------


async def check_one_plugin(
    *,
    install: PluginInstall,
    supervisor: PluginSupervisor,
    http_client: httpx.AsyncClient,
    convex_url: str,
    api_key: str | None,
    device_id: str,
    current_board_id: str | None,
) -> AutoUpdateOutcome:
    """Evaluate one plugin against the registry and act.

    See module docstring for the full decision tree. Always returns
    an outcome — never raises — so the daily loop can iterate the
    full set even when one plugin's poll fails.
    """
    plugin_id = install.plugin_id

    # Gate 1: pinned or auto-update disabled.
    if install.pinned_version is not None:
        log.info(
            "auto_update_skip_pinned",
            plugin_id=plugin_id,
            pinned=install.pinned_version,
        )
        return AutoUpdateOutcome.SKIPPED
    if not install.auto_update:
        log.info("auto_update_skip_disabled", plugin_id=plugin_id)
        return AutoUpdateOutcome.SKIPPED

    # Gate 2: query the registry.
    try:
        payload = await _registry_get_plugin(
            http_client, convex_url, api_key, plugin_id
        )
    except (httpx.HTTPError, RuntimeError, json.JSONDecodeError) as exc:
        log.warning(
            "auto_update_registry_query_failed",
            plugin_id=plugin_id,
            error=str(exc),
        )
        _record_attempt(install, supervisor, version="unknown", outcome="failed", error=str(exc))
        return AutoUpdateOutcome.FAILED

    if not payload:
        log.debug("auto_update_no_registry_row", plugin_id=plugin_id)
        return AutoUpdateOutcome.SKIPPED

    version_row = _pick_latest_version_row(payload)
    if version_row is None:
        log.debug("auto_update_no_versions", plugin_id=plugin_id)
        return AutoUpdateOutcome.SKIPPED

    latest_version = str(version_row.get("version") or "")
    if not latest_version:
        log.warning(
            "auto_update_version_row_missing_version", plugin_id=plugin_id
        )
        return AutoUpdateOutcome.SKIPPED

    # Gate 3: is there actually a newer version?
    if not _is_newer(latest_version, install.version):
        log.debug(
            "auto_update_already_current",
            plugin_id=plugin_id,
            current=install.version,
            latest=latest_version,
        )
        return AutoUpdateOutcome.SKIPPED

    # Gate 4: parse the remote manifest so we can compare permissions.
    remote_manifest = _parse_version_manifest(version_row)
    if remote_manifest is None:
        return AutoUpdateOutcome.SKIPPED

    # Gate 5: major bump → notify.
    if _is_major_bump(latest_version, install.version):
        notice = {
            "plugin_id": plugin_id,
            "current_version": install.version,
            "latest_version": latest_version,
            "reason": "major_bump",
            "timestamp_ms": _now_ms(),
        }
        _publish_update_notice(device_id, notice)
        _record_attempt(
            install, supervisor, version=latest_version, outcome="notify", error=None
        )
        log.info(
            "auto_update_notify_major",
            plugin_id=plugin_id,
            current=install.version,
            latest=latest_version,
        )
        return AutoUpdateOutcome.NOTIFY

    # Gate 6: board support check.
    supported_boards = version_row.get("supported_boards") or []
    if (
        supported_boards
        and current_board_id
        and current_board_id not in supported_boards
    ):
        notice = {
            "plugin_id": plugin_id,
            "current_version": install.version,
            "latest_version": latest_version,
            "reason": "board_mismatch",
            "timestamp_ms": _now_ms(),
        }
        _publish_update_notice(device_id, notice)
        _record_attempt(
            install, supervisor, version=latest_version, outcome="notify", error=None
        )
        log.info(
            "auto_update_notify_board_mismatch",
            plugin_id=plugin_id,
            board=current_board_id,
        )
        return AutoUpdateOutcome.NOTIFY

    # Gate 7: permission delta. Set-equality on declared id sets.
    # New permissions in the remote manifest that the operator never
    # approved on this device require a fresh approval dialog.
    current_declared = set(install.permissions.keys())
    remote_declared = remote_manifest.declared_permissions()
    new_permissions = sorted(remote_declared - current_declared)
    if new_permissions:
        notice = {
            "plugin_id": plugin_id,
            "current_version": install.version,
            "latest_version": latest_version,
            "reason": "permission_delta",
            "new_permissions": new_permissions,
            "timestamp_ms": _now_ms(),
        }
        _publish_update_notice(device_id, notice)
        _record_attempt(
            install, supervisor, version=latest_version, outcome="notify", error=None
        )
        log.info(
            "auto_update_notify_permission_delta",
            plugin_id=plugin_id,
            new_permissions=new_permissions,
        )
        return AutoUpdateOutcome.NOTIFY

    # Gate 8: silent install path.
    return await _silent_install(
        install=install,
        supervisor=supervisor,
        http_client=http_client,
        version_row=version_row,
        latest_version=latest_version,
    )


async def _silent_install(
    *,
    install: PluginInstall,
    supervisor: PluginSupervisor,
    http_client: httpx.AsyncClient,
    version_row: dict,
    latest_version: str,
) -> AutoUpdateOutcome:
    """Download, replace, and re-enable a plugin atomically.

    Sequence: ``disable`` → ``remove(keep_data=False)`` →
    ``install_archive`` → ``enable`` → re-grant every previously
    granted permission. If install_archive raises before any state
    mutation, re-enable the original plugin so the operator does not
    find their plugin stuck in disabled state. The supervisor's
    install path is atomic — it raises before mutating state when
    signature / manifest / compatibility checks fail, so there is
    nothing for us to roll back beyond restoring the enabled flag.
    """
    plugin_id = install.plugin_id
    download_url = str(version_row.get("download_url") or "")
    expected_sha256 = str(version_row.get("archive_sha256") or "")
    granted_perm_ids = [
        pid for pid, grant in install.permissions.items() if grant.granted
    ]

    if not download_url:
        log.warning(
            "auto_update_no_download_url",
            plugin_id=plugin_id,
            version=latest_version,
        )
        _record_attempt(
            install,
            supervisor,
            version=latest_version,
            outcome="failed",
            error="no download_url on registry row",
        )
        return AutoUpdateOutcome.FAILED

    # ---- Step 1: download to a temp file --------------------------------
    try:
        validate_download_url(download_url)
        status_code, archive_bytes = await stream_download(
            http_client, download_url
        )
        if status_code != 200:
            raise DownloadError(f"download HTTP {status_code}")
        verify_sha256(archive_bytes, expected_sha256)
    except (DownloadError, httpx.HTTPError) as exc:
        log.warning(
            "auto_update_download_failed",
            plugin_id=plugin_id,
            error=str(exc),
        )
        _record_attempt(
            install,
            supervisor,
            version=latest_version,
            outcome="failed",
            error=f"download: {exc}",
        )
        return AutoUpdateOutcome.FAILED

    with tempfile.NamedTemporaryFile(
        suffix=".adosplug", delete=False
    ) as tmp:
        tmp.write(archive_bytes)
        tmp_path = Path(tmp.name)

    disabled_for_swap = False
    try:
        # ---- Step 2: stop the running plugin ---------------------------
        try:
            await asyncio.to_thread(supervisor.disable, plugin_id)
            disabled_for_swap = True
        except SupervisorError as exc:
            log.warning(
                "auto_update_disable_failed",
                plugin_id=plugin_id,
                error=str(exc),
            )
            # If we cannot stop the plugin we cannot safely replace it.
            _record_attempt(
                install,
                supervisor,
                version=latest_version,
                outcome="failed",
                error=f"disable: {exc}",
            )
            return AutoUpdateOutcome.FAILED

        try:
            await asyncio.to_thread(
                supervisor.remove, plugin_id, keep_data=False
            )
        except SupervisorError as exc:
            log.warning(
                "auto_update_remove_failed",
                plugin_id=plugin_id,
                error=str(exc),
            )
            _record_attempt(
                install,
                supervisor,
                version=latest_version,
                outcome="failed",
                error=f"remove: {exc}",
            )
            return AutoUpdateOutcome.FAILED

        # ---- Step 3: install the new archive ---------------------------
        try:
            await asyncio.to_thread(supervisor.install_archive, tmp_path)
        except (
            SignatureError,
            ManifestError,
            ArchiveError,
            SupervisorError,
        ) as exc:
            log.warning(
                "auto_update_install_failed",
                plugin_id=plugin_id,
                error=str(exc),
            )
            _record_attempt(
                install,
                supervisor,
                version=latest_version,
                outcome="failed",
                error=f"install: {exc}",
            )
            return AutoUpdateOutcome.FAILED

        # ---- Step 4: re-grant prior permissions ------------------------
        for perm_id in granted_perm_ids:
            try:
                await asyncio.to_thread(
                    supervisor.grant_permission, plugin_id, perm_id
                )
            except SupervisorError as exc:
                # A grant failure is non-fatal — the plugin is
                # installed and the operator can re-grant later from
                # the GCS perms panel.
                log.warning(
                    "auto_update_regrant_failed",
                    plugin_id=plugin_id,
                    permission=perm_id,
                    error=str(exc),
                )

        # ---- Step 5: re-enable ----------------------------------------
        try:
            await asyncio.to_thread(supervisor.enable, plugin_id)
        except SupervisorError as exc:
            log.warning(
                "auto_update_enable_failed",
                plugin_id=plugin_id,
                error=str(exc),
            )
            _record_attempt(
                install,
                supervisor,
                version=latest_version,
                outcome="failed",
                error=f"enable: {exc}",
            )
            return AutoUpdateOutcome.FAILED

        log.info(
            "auto_update_silent_install_ok",
            plugin_id=plugin_id,
            from_version=install.version,
            to_version=latest_version,
        )
        _record_attempt(
            install,
            supervisor,
            version=latest_version,
            outcome="success",
            error=None,
        )
        return AutoUpdateOutcome.SILENT_INSTALL
    finally:
        try:
            tmp_path.unlink(missing_ok=True)
        except OSError as exc:
            log.warning("auto_update_tmp_cleanup_failed", error=str(exc))


def _record_attempt(
    install: PluginInstall,
    supervisor: PluginSupervisor,
    *,
    version: str,
    outcome: str,
    error: str | None,
) -> None:
    """Persist the most recent attempt on the install record.

    Reload state through the supervisor before mutating: the install
    swap path may have replaced the in-memory install with a fresh
    instance that does not have our local handle's attempt fields.
    Always write under ``state_lock`` to avoid racing the CLI.
    """
    with state_lock():
        live = supervisor.find_install(install.plugin_id)
        target = live if live is not None else install
        target.last_update_attempt = {
            "version": version,
            "outcome": outcome,
            "error": error,
        }
        save_state(supervisor.installs())


# ---------------------------------------------------------------------
# Daily loop
# ---------------------------------------------------------------------


def _next_sleep_seconds() -> float:
    """Daily interval +/- one hour of jitter."""
    return DAILY_SLEEP_SECONDS + random.randint(
        -JITTER_RANGE_SECONDS, JITTER_RANGE_SECONDS
    )


async def _sleep_with_shutdown(
    seconds: float, shutdown: asyncio.Event
) -> bool:
    """Sleep up to ``seconds`` or until shutdown fires. Returns True on shutdown."""
    try:
        await asyncio.wait_for(shutdown.wait(), timeout=seconds)
    except asyncio.TimeoutError:
        return False
    return True


async def run_daily_loop(ctx: Any) -> None:
    """Iterate enabled installs once per day and act per the decision tree.

    The loop never raises; every per-plugin failure is captured and
    logged. The outer ``while`` exits cleanly on shutdown.
    """
    config = ctx.config
    convex_url = ctx.convex_url
    pairing = ctx.pairing
    shutdown = ctx.shutdown
    board = ctx.board
    device_id = config.agent.device_id
    current_board_id = board.name if board else None

    log.info("auto_update_loop_started")

    supervisor = PluginSupervisor(current_board_id=current_board_id)
    try:
        supervisor.discover()
    except Exception as exc:  # noqa: BLE001
        log.warning("auto_update_supervisor_discover_failed", error=str(exc))
        return

    while not shutdown.is_set():
        # Defer first-run polling by a short delay so we are not
        # racing the rest of cloud boot.
        if await _sleep_with_shutdown(60.0, shutdown):
            break

        if not (pairing.is_paired and convex_url):
            # Without pairing we have no credentials for the registry.
            # Skip this cycle, recheck again after the daily sleep.
            log.debug("auto_update_skip_unpaired")
        else:
            await _run_one_cycle(
                supervisor=supervisor,
                convex_url=convex_url,
                api_key=pairing.api_key,
                device_id=device_id,
                current_board_id=current_board_id,
            )

        if await _sleep_with_shutdown(_next_sleep_seconds(), shutdown):
            break

    log.info("auto_update_loop_stopped")


async def _run_one_cycle(
    *,
    supervisor: PluginSupervisor,
    convex_url: str,
    api_key: str | None,
    device_id: str,
    current_board_id: str | None,
) -> None:
    """One pass over every enabled install."""
    installs = [
        i
        for i in supervisor.installs()
        if i.status in ("enabled", "running")
    ]
    if not installs:
        log.debug("auto_update_no_enabled_installs")
        return

    async with httpx.AsyncClient(timeout=REGISTRY_TIMEOUT_SECONDS) as client:
        for install in installs:
            try:
                outcome = await check_one_plugin(
                    install=install,
                    supervisor=supervisor,
                    http_client=client,
                    convex_url=convex_url,
                    api_key=api_key,
                    device_id=device_id,
                    current_board_id=current_board_id,
                )
            except Exception as exc:  # noqa: BLE001
                # Defense-in-depth: check_one_plugin is supposed to
                # never raise, but if a future edit breaks that we
                # still want the loop to keep going.
                log.warning(
                    "auto_update_plugin_check_crashed",
                    plugin_id=install.plugin_id,
                    error=str(exc),
                )
                outcome = AutoUpdateOutcome.FAILED

            # Always stamp the last_check timestamp on the live install
            # record so the GCS sees the loop is alive even when the
            # outcome was a no-op.
            with state_lock():
                live = supervisor.find_install(install.plugin_id)
                if live is not None:
                    live.last_update_check_at = _now_ms()
                    save_state(supervisor.installs())
            log.debug(
                "auto_update_plugin_checked",
                plugin_id=install.plugin_id,
                outcome=outcome.value,
            )


def latest_check_timestamp_ms() -> int | None:
    """Return the most recent ``last_update_check_at`` across all installs.

    Used by the heartbeat composer to surface a fleet-wide freshness
    indicator. Returns ``None`` when no install has ever been checked
    (fresh agent or auto-update never ran).
    """
    try:
        installs = load_state()
    except Exception:  # noqa: BLE001
        return None
    timestamps = [
        i.last_update_check_at for i in installs if i.last_update_check_at
    ]
    if not timestamps:
        return None
    return max(timestamps)


__all__ = [
    "AutoUpdateOutcome",
    "check_one_plugin",
    "latest_check_timestamp_ms",
    "run_daily_loop",
]
