"""Pair-state manager for the WFB radio link.

Disambiguation: this module is the WFB radio-link pair manager (drone
↔ ground-station key state). It is NOT the mesh tap-to-pair manager
for joining relays into a deployment — that lives in
``pairing_manager`` (note the ``-ing`` suffix). Different concern,
different transport, similar filename.

This module owns the persisted "are these two rigs paired" state on
either side of the link. The actual key bytes come from `wfb_keygen`
via `ados.services.wfb.key_mgr` (during a local bind window or a
cloud-relay handshake) and reach this module pre-formed; the manager
writes them atomically to `/etc/ados/wfb/{tx,rx}.key`, persists the
peer device-id + paired timestamp to `/etc/ados/config.yaml`, and
signals the appropriate wfb systemd unit to pick up the new keys.

Trigger surfaces:
- Local bind orchestrator (auto-pair on first boot or operator-driven
  bind window). The orchestrator hands a 64-byte blob to
  `apply_keypair()`.
- Cloud-relay command handlers (`wfb_pair_init_remote` /
  `wfb_pair_apply_remote`). Same call, different transport.
- Long-press B3 on the ground-station LCD (kicks the orchestrator).
- POST `/api/wfb/pair/...` REST routes.

The legacy "user-typed shared-key string -> SHA-256 -> 32-byte rx.key"
path was a POC and is gone. The wire format wfb-ng requires is the
64-byte libsodium crypto_box keypair file produced by `wfb_keygen`.
Anything else fails decryption silently.
"""

from __future__ import annotations

import asyncio
import os
import subprocess
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Literal

import yaml

from ados.core.logging import get_logger
from ados.core.paths import AP_PASSPHRASE_PATH, CONFIG_YAML
from ados.services.wfb.key_mgr import (
    WFB_KEY_FILE_BYTES,
    get_key_paths,
    read_public_fingerprint,
)

log = get_logger("ground_station.pair_manager")

_SETUP_COMPLETE_PATH = Path("/var/lib/ados/setup-complete")
_AP_PASSPHRASE_PATH = AP_PASSPHRASE_PATH
_CONFIG_PATH = CONFIG_YAML

_WFB_DRONE_UNIT = "ados-wfb.service"
_WFB_GS_UNIT = "ados-wfb-rx.service"

Role = Literal["drone", "gs"]


def _iso_now() -> str:
    """Return the current UTC timestamp in ISO 8601 form."""
    return datetime.now(UTC).isoformat(timespec="seconds")


def _atomic_write(path: Path, data: bytes, mode: int = 0o600) -> None:
    """Write `data` to `path` atomically with a specific file mode.

    Writes to a sibling temp file, chmods, then renames onto the final
    path. Creates the parent directory if missing.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = path.with_suffix(path.suffix + ".tmp")

    fd = os.open(
        str(tmp_path),
        os.O_CREAT | os.O_WRONLY | os.O_TRUNC,
        mode,
    )
    try:
        os.write(fd, data)
        os.fsync(fd)
    finally:
        os.close(fd)

    os.chmod(tmp_path, mode)
    os.rename(tmp_path, path)


_CONFIG_LOCK_PATH = Path("/run/ados/config.yaml.lock")


def _load_config_dict() -> dict[str, Any]:
    """Load `/etc/ados/config.yaml` as a raw dict, tolerating absence."""
    if not _CONFIG_PATH.is_file():
        return {}
    try:
        with open(_CONFIG_PATH, encoding="utf-8") as fh:
            loaded = yaml.safe_load(fh)
        if isinstance(loaded, dict):
            return loaded
    except (OSError, yaml.YAMLError) as exc:
        log.warning("config_read_failed", path=str(_CONFIG_PATH), error=str(exc))
    return {}


def _save_config_dict(data: dict[str, Any]) -> bool:
    """Atomically rewrite `/etc/ados/config.yaml` with the given dict.

    mode 0o600 because this file carries secrets (mqtt_password,
    api_key, hmac_secret, pair fingerprints).

    Serialized via a `/run/ados/config.yaml.lock` flock so concurrent
    PUTs from two GCS tabs or a GCS+CLI race do not silently lose the
    first write. The flock window covers only the YAML serialize +
    atomic-write (the caller has already loaded + mutated their own
    copy); a true compare-and-swap that re-reads under the lock is
    out of scope here because every callsite already round-trips
    `_load_config_dict()` immediately before this call.

    Requires euid 0 because the file is created mode 0o600 owned by
    root. A non-root caller will get EPERM from `open()`; we surface
    that as a clear log line rather than a silent False.
    """
    import fcntl

    if os.geteuid() != 0:
        log.error(
            "config_write_requires_root",
            path=str(_CONFIG_PATH),
            euid=os.geteuid(),
        )
        return False

    try:
        _CONFIG_LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
        lock_fd = os.open(
            str(_CONFIG_LOCK_PATH),
            os.O_CREAT | os.O_WRONLY,
            0o600,
        )
    except OSError as exc:
        log.error(
            "config_lock_open_failed",
            path=str(_CONFIG_LOCK_PATH),
            error=str(exc),
        )
        # Fall through to the write anyway. Losing the lock is worse
        # than racing because the file system primitives are still
        # atomic at the rename level.
        lock_fd = -1

    # Snapshot the on-disk config before overwriting it, so the post-write
    # sync can tell whether the CRSF lane slice actually changed (best-effort;
    # a missing/garbled file reads as no previous config).
    previous_config: dict[str, Any] | None
    try:
        loaded = yaml.safe_load(_CONFIG_PATH.read_text())
        previous_config = loaded if isinstance(loaded, dict) else None
    except (OSError, yaml.YAMLError):
        previous_config = None

    try:
        if lock_fd >= 0:
            fcntl.flock(lock_fd, fcntl.LOCK_EX)
        try:
            body = yaml.safe_dump(
                data, sort_keys=False, default_flow_style=False,
            )
            _atomic_write(_CONFIG_PATH, body.encode("utf-8"), mode=0o600)
            # Keep the CRSF lane's enable marker + unit true to the persisted
            # config (the marker mirrors radio.crsf.enabled; the unit gets a
            # no-block reload-or-restart only when the lane slice changed).
            # Best-effort by contract: a marker/systemctl hiccup never fails
            # the write that already landed.
            try:
                from ados.core.crsf_marker import sync_after_config_write

                sync_after_config_write(previous_config, data)
            except Exception as exc:  # noqa: BLE001 — the write already landed
                log.warning("crsf_config_sync_failed", error=str(exc))
            return True
        except (OSError, yaml.YAMLError) as exc:
            log.error(
                "config_write_failed",
                path=str(_CONFIG_PATH),
                error=str(exc),
            )
            return False
    finally:
        if lock_fd >= 0:
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_UN)
            except OSError:
                pass
            try:
                os.close(lock_fd)
            except OSError:
                pass


def _get_section(data: dict[str, Any], key: str) -> dict[str, Any]:
    section = data.get(key)
    if not isinstance(section, dict):
        section = {}
        data[key] = section
    return section


def _get_video_wfb_section(data: dict[str, Any]) -> dict[str, Any]:
    """Walk to `video.wfb` in the raw dict, materializing missing levels."""
    video = _get_section(data, "video")
    return _get_section(video, "wfb")


def update_peer_device_id(role: Role, peer_device_id: str) -> bool:
    """Set peer_device_id on the persisted pair state without disturbing
    other fields (paired_at, auto_pair_enabled, key path references).

    Used by the presence-beacon path to back-fill the peer identifier
    after a pair where the bind tunnel did not carry it. Idempotent:
    returns False if the value is already correct, True if the config
    was rewritten so callers can decide whether to log.
    """
    data = _load_config_dict()
    wfb = _get_video_wfb_section(data)
    if wfb.get("paired_with_device_id") == peer_device_id:
        return False
    wfb["paired_with_device_id"] = peer_device_id
    if role == "gs":
        gs = _get_section(data, "ground_station")
        gs["paired_drone_id"] = peer_device_id
    _save_config_dict(data)
    return True


def _persist_pair_state(
    *,
    role: Role,
    peer_device_id: str | None,
    paired_at: str | None,
    auto_pair_enabled: bool | None = None,
) -> None:
    """Update the persisted pair fields under `video.wfb` (canonical) and
    mirror onto `ground_station.paired_drone_id` / `paired_at` for the GS
    profile so older code paths that read the legacy fields keep working.
    """
    data = _load_config_dict()
    wfb = _get_video_wfb_section(data)

    if peer_device_id is None:
        wfb.pop("paired_with_device_id", None)
    else:
        wfb["paired_with_device_id"] = peer_device_id

    if paired_at is None:
        wfb.pop("paired_at", None)
    else:
        wfb["paired_at"] = paired_at

    if auto_pair_enabled is not None:
        wfb["auto_pair_enabled"] = bool(auto_pair_enabled)

    if role == "gs":
        gs = _get_section(data, "ground_station")
        if peer_device_id is None:
            gs.pop("paired_drone_id", None)
            gs.pop("paired_at", None)
        else:
            gs["paired_drone_id"] = peer_device_id
            gs["paired_at"] = paired_at

    _save_config_dict(data)


def _systemctl(action: str, unit: str) -> bool:
    """Thin wrapper around `systemctl <action> <unit>`."""
    try:
        result = subprocess.run(
            ["systemctl", action, unit],
            check=False,
            capture_output=True,
            timeout=10,
        )
        if result.returncode != 0:
            log.warning(
                "systemctl_nonzero",
                action=action,
                unit=unit,
                rc=result.returncode,
                stderr=result.stderr.decode(errors="replace").strip(),
            )
            return False
        return True
    except (OSError, subprocess.SubprocessError) as exc:
        log.warning("systemctl_failed", action=action, unit=unit, error=str(exc))
        return False


class PairKeyError(ValueError):
    """Raised when a key blob fails the format check."""


def _validate_blob(blob: bytes) -> None:
    if not isinstance(blob, (bytes, bytearray)):
        raise PairKeyError("key blob must be bytes")
    if len(blob) != WFB_KEY_FILE_BYTES:
        raise PairKeyError(
            f"key blob is {len(blob)} bytes, expected {WFB_KEY_FILE_BYTES}"
        )


class PairManager:
    """WFB pair-state manager.

    Single instance per agent. Both drone-profile and ground-station
    profile use the same manager; the role is supplied per call. All
    operations are async for API symmetry even though the underlying
    file and subprocess work is synchronous.
    """

    def __init__(self, key_dir: str | None = None) -> None:
        tx_path, rx_path = get_key_paths(key_dir)
        self._tx_key_path = Path(tx_path)
        self._rx_key_path = Path(rx_path)

    @property
    def tx_key_path(self) -> Path:
        return self._tx_key_path

    @property
    def rx_key_path(self) -> Path:
        return self._rx_key_path

    def _key_path_for_role(self, role: Role) -> Path:
        # Drone profile keeps the air-side file (drone.key from
        # wfb_keygen → tx.key here). GS profile keeps gs.key → rx.key.
        return self._tx_key_path if role == "drone" else self._rx_key_path

    def _wfb_unit_for_role(self, role: Role) -> str:
        return _WFB_DRONE_UNIT if role == "drone" else _WFB_GS_UNIT

    async def apply_keypair(
        self,
        blob: bytes,
        role: Role,
        peer_device_id: str | None = None,
    ) -> dict[str, Any]:
        """Persist an inbound 64-byte wfb-ng key file.

        Args:
            blob: Raw 64-byte libsodium crypto_box keypair (from
                `wfb_keygen` on the peer or from the cloud relay).
            role: `"drone"` writes the blob to `tx.key`, `"gs"` writes
                it to `rx.key`. Determines which systemd unit gets
                reloaded too.
            peer_device_id: Optional device-id of the paired peer.
                Persisted to config for UI display and cross-rig
                fingerprint cross-check.

        Returns:
            Dict with `paired`, `paired_with_device_id`, `paired_at`,
            `fingerprint`, `role`.

        Raises:
            PairKeyError: If `blob` is the wrong shape.
        """
        _validate_blob(blob)

        target = self._key_path_for_role(role)
        _atomic_write(target, bytes(blob), mode=0o600)
        fingerprint = read_public_fingerprint(target)
        paired_at = _iso_now()

        # Reaching here means the bind tunnel completed the key transfer
        # and a valid keypair is now on disk, so this is a real pair —
        # disarm auto_pair regardless of whether a peer device-id was
        # exchanged. The local radio-bind protocol does not carry a
        # device-id, so gating the disarm on peer_device_id left every
        # local bind armed: the next boot re-ran auto_pair, which wipes the
        # freshly written tx.key/rx.key, and the pairing never survived a
        # reboot. The device-id remains optional metadata for UI display
        # and the fingerprint cross-check; the link does not need it.
        _persist_pair_state(
            role=role,
            peer_device_id=peer_device_id,
            paired_at=paired_at,
            auto_pair_enabled=False,
        )

        # Drop the setup-complete sentinel so captive_dns.py stops
        # redirecting. Best-effort on the GS side; harmless on drone.
        try:
            _atomic_write(
                _SETUP_COMPLETE_PATH,
                (paired_at + "\n").encode("utf-8"),
                mode=0o644,
            )
        except OSError as exc:
            log.warning(
                "setup_complete_sentinel_failed",
                path=str(_SETUP_COMPLETE_PATH),
                error=str(exc),
            )

        unit = self._wfb_unit_for_role(role)
        # restart over reload: WfbManager waits in the unpaired loop
        # until keys appear, but it samples key existence on its own
        # backoff cadence. A unit restart is the prompt path to a new
        # spawn cycle that picks up the freshly written file.
        if not _systemctl("restart", unit):
            log.info(
                "wfb_unit_restart_skipped",
                unit=unit,
                note="unit may not be active yet, keys will be picked up on next start",
            )

        log.info(
            "pair_complete",
            role=role,
            peer_device_id=peer_device_id or "unknown",
            fingerprint=fingerprint,
            paired_at=paired_at,
        )

        return {
            "paired": True,
            "paired_with_device_id": peer_device_id,
            "paired_at": paired_at,
            "fingerprint": fingerprint,
            "role": role,
        }

    async def unpair(self, role: Role) -> dict[str, Any]:
        """Wipe both key files and clear persisted pair state.

        Leaves `auto_pair_enabled = False` so the rig does not silently
        re-bind to a different peer. Operator must re-arm explicitly.
        """
        # Always wipe BOTH files even on a single-role rig: a stale
        # rx.key on a drone (or stale tx.key on a GS) would never be
        # used in normal operation, but it leaks crypto material on
        # disk and confuses the heartbeat surface.
        for path in (self._tx_key_path, self._rx_key_path):
            try:
                if path.is_file():
                    path.unlink()
            except OSError as exc:
                log.warning(
                    "key_delete_failed",
                    path=str(path),
                    error=str(exc),
                )

        _persist_pair_state(
            role=role,
            peer_device_id=None,
            paired_at=None,
            auto_pair_enabled=False,
        )

        unit = self._wfb_unit_for_role(role)
        _systemctl("restart", unit)

        log.warning("unpair_complete", role=role)

        return {
            "paired": False,
            "role": role,
        }

    async def recover_half_pair_state(self, role: Role) -> dict[str, Any]:
        """Detect and recover from a stuck half-pair.

        A half-pair is when the local bind protocol wrote a key file
        and stamped `paired_at` but never learned the peer's device id.
        From the rig's own perspective it looks paired; from the peer's
        perspective the bind aborted before its half landed. auto_pair
        is disarmed on both sides and nothing climbs out without
        operator intervention.

        Recovery: if a key file is present but `paired_with_device_id`
        is None / "unknown", treat the rig as never having paired.
        Delete the orphan key file, clear the stale `paired_at`, and
        leave `auto_pair_enabled` armed so the supervisor picks up the
        next bind cycle on its own.

        Returns a dict with `recovered: bool` and the cleared fields.
        Safe to call on a healthy rig (where it returns recovered=False
        without touching anything).
        """
        target = self._key_path_for_role(role)
        if not (target.is_file() and target.stat().st_size == WFB_KEY_FILE_BYTES):
            return {"recovered": False, "reason": "no_key_file"}

        cfg = _load_config_dict()
        wfb_section = (
            cfg.get("video", {}).get("wfb", {})
            if isinstance(cfg.get("video"), dict) else {}
        )
        peer = wfb_section.get("paired_with_device_id")
        # A local radio bind never records a peer device-id (the bind
        # protocol exchanges keys, not ADOS device ids), so an unknown
        # peer is NOT evidence of a half-pair — it is the normal shape of
        # a successful local bind. A valid-sized key file on disk means the
        # rig paired; keep it. Deleting it here on every boot was what made
        # local pairings evaporate across reboots: the key vanished, auto
        # pair re-armed, and the rig rebound (re-keying away from its peer)
        # on every restart. A cloud-relay pairing does carry a device-id
        # and is equally healthy. Either way a present valid key is left
        # untouched; a genuinely broken pairing is cleared by the operator
        # with `ados radio pair unpair`, not silently wiped on boot.
        reason = (
            "real_peer_known"
            if isinstance(peer, str) and peer and peer != "unknown"
            else "local_bind_no_peer_id"
        )
        return {"recovered": False, "reason": reason}

    async def status(self, role: Role) -> dict[str, Any]:
        """Return live pair status for the given role.

        Fields: paired, paired_with_device_id, paired_at, fingerprint,
        auto_pair_enabled, role.
        """
        target = self._key_path_for_role(role)
        paired = target.is_file() and target.stat().st_size == WFB_KEY_FILE_BYTES
        fingerprint: str | None = None
        if paired:
            try:
                fingerprint = read_public_fingerprint(target)
            except (OSError, ValueError) as exc:
                log.debug("fingerprint_read_failed", path=str(target), error=str(exc))
                paired = False

        cfg = _load_config_dict()
        wfb_section = cfg.get("video", {}).get("wfb", {}) if isinstance(cfg.get("video"), dict) else {}
        peer = wfb_section.get("paired_with_device_id")
        paired_at = wfb_section.get("paired_at")
        auto_pair_enabled = bool(wfb_section.get("auto_pair_enabled", True))

        # GS-profile fallback: a rig migrated from a pre-0.16 config may
        # still carry pair state under ground_station.* without the new
        # video.wfb.* mirror. Read both, prefer the canonical spot.
        if role == "gs" and peer is None:
            gs = cfg.get("ground_station") if isinstance(cfg.get("ground_station"), dict) else {}
            peer = gs.get("paired_drone_id") if isinstance(gs, dict) else None
            if paired_at is None and isinstance(gs, dict):
                paired_at = gs.get("paired_at")

        return {
            "paired": paired,
            "paired_with_device_id": peer if isinstance(peer, str) else None,
            "paired_at": paired_at if isinstance(paired_at, str) else None,
            "fingerprint": fingerprint,
            "auto_pair_enabled": auto_pair_enabled,
            "role": role,
        }

    async def set_auto_pair(self, enabled: bool, role: Role) -> dict[str, Any]:
        """Toggle the persisted auto_pair_enabled flag.

        Re-arming on a rig that's already paired is a no-op + a
        warning; the operator must `unpair` first to clear pair state
        before auto-bind can run again.
        """
        current = await self.status(role)
        if enabled and current["paired"]:
            log.warning(
                "auto_pair_rearm_blocked_while_paired",
                role=role,
                peer=current.get("paired_with_device_id"),
            )
            return {**current, "auto_pair_enabled": False, "rearm_blocked": True}

        _persist_pair_state(
            role=role,
            peer_device_id=current.get("paired_with_device_id"),
            paired_at=current.get("paired_at"),
            auto_pair_enabled=enabled,
        )

        log.info("auto_pair_set", enabled=enabled, role=role)
        return {**current, "auto_pair_enabled": enabled}

    async def factory_reset(self, role: Role) -> dict[str, Any]:
        """Wipe pair keys, setup sentinel, AP passphrase. Re-arm auto-pair.

        Forces the rig back to first-boot posture: captive DNS will fire
        again, hostapd_manager regenerates a fresh passphrase, the
        operator re-runs the setup webapp flow, and auto-bind is armed
        for the next boot.
        """
        await self.unpair(role)

        for path in (_SETUP_COMPLETE_PATH, _AP_PASSPHRASE_PATH):
            try:
                if path.is_file():
                    path.unlink()
            except OSError as exc:
                log.warning(
                    "factory_reset_delete_failed",
                    path=str(path),
                    error=str(exc),
                )

        # Re-arm auto-pair so the next boot binds again.
        _persist_pair_state(
            role=role,
            peer_device_id=None,
            paired_at=None,
            auto_pair_enabled=True,
        )

        ts = _iso_now()
        log.warning("factory_reset_performed", role=role, timestamp=ts)
        return {"reset": True, "timestamp": ts, "auto_pair_enabled": True}


# ---------------------------------------------------------------------
# Module-level singleton
# ---------------------------------------------------------------------
_instance: PairManager | None = None


def get_pair_manager() -> PairManager:
    """Return the process-wide PairManager singleton."""
    global _instance
    if _instance is None:
        _instance = PairManager()
    return _instance


def _reset_for_tests() -> None:
    """Drop the cached singleton. Test-only helper."""
    global _instance
    _instance = None


# Convenience for callers that already have an event loop:
async def apply_drone_keypair(
    blob: bytes, peer_device_id: str | None = None
) -> dict[str, Any]:
    return await get_pair_manager().apply_keypair(blob, "drone", peer_device_id)


async def apply_gs_keypair(
    blob: bytes, peer_device_id: str | None = None
) -> dict[str, Any]:
    return await get_pair_manager().apply_keypair(blob, "gs", peer_device_id)


# Sync wrappers for callers outside an event loop (CLI, install hooks).
def apply_keypair_sync(
    blob: bytes, role: Role, peer_device_id: str | None = None
) -> dict[str, Any]:
    return asyncio.run(get_pair_manager().apply_keypair(blob, role, peer_device_id))
