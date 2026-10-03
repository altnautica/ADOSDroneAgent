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

from ados.core.config.writer import read_config_mapping, update_config
from ados.core.logging import get_logger
from ados.core.paths import (
    CONFIG_YAML,
    SECRETS_DIR,
    SETUP_COMPLETE_PATH,
)
from ados.services.wfb.key_mgr import (
    WFB_KEY_FILE_BYTES,
    get_key_paths,
    read_public_fingerprint,
)

log = get_logger("ground_station.pair_manager")

_SETUP_COMPLETE_PATH = SETUP_COMPLETE_PATH

_CONFIG_PATH = CONFIG_YAML

# The relay peer secret a ground station offered over the radio. It belongs to
# the radio pairing it was offered under, so an unpair drops it: the next
# ground station's offer is then accepted rather than refused as already held.
_RELAY_SECRET_PATH = SECRETS_DIR / "relay-peer-secret"

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


def _load_config_dict() -> dict[str, Any]:
    """Load `/etc/ados/config.yaml` as a raw dict, tolerating absence.

    Read-only projection for the status paths below. A read that feeds a WRITE
    must not use this: `update_config` takes its own read inside the write
    lock, which is the only read a write may rely on.
    """
    return read_config_mapping(_CONFIG_PATH)


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

    Goes through the one config writer, so the read that feeds the write is
    taken inside the write lock: an unpair racing an operator's settings PUT
    can no longer resurrect the peer id the unpair just removed.
    """
    def _apply(data: dict[str, Any]) -> None:
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

    result = update_config(_apply, path=_CONFIG_PATH)
    if not result:
        log.error(
            "pair_state_persist_failed",
            role=role,
            peer_device_id=peer_device_id,
            error=result.error,
        )

    # The audit trail: a pair transition is what admits a peer to the fleet (or
    # removes it), and the radio keypair it rides on is the fleet's join gate. An
    # operator asking later "when did this ground station adopt that drone" has
    # only this record and the config's own `paired_at` to work from.
    from ados.core import audit

    audit.record(
        audit.PAIRING_STATE_CHANGED,
        audit.ACTOR_OPERATOR,
        {
            "role": role,
            "peer_device_id": peer_device_id,
            "paired": peer_device_id is not None,
            "paired_at": paired_at,
            "auto_pair_enabled": auto_pair_enabled,
        },
    )


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
        if not await asyncio.to_thread(_systemctl, "restart", unit):
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
        for path in (self._tx_key_path, self._rx_key_path, _RELAY_SECRET_PATH):
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
        await asyncio.to_thread(_systemctl, "restart", unit)

        log.warning("unpair_complete", role=role)

        return {
            "paired": False,
            "role": role,
        }

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
