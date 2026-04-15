"""Pair key manager for the ground-station profile (MSN-025, DEC-112).

The ground station and the paired drone share a WFB-ng keypair. The drone
is provisioned with a tx key, the ground station with a matching rx key.
This module owns the rx-side keypair for the ground station: writing
`/etc/ados/wfb/rx.key` and `/etc/ados/wfb/rx.key.pub` atomically, tracking
which drone is paired in agent config, signalling `ados-wfb-rx` to pick
up the new keys, and dropping the setup-complete sentinel so the captive
DNS responder (Wave B) can shut itself down.

Pair triggers:
- Long-press B3 on the front-panel OLED (spec 05, Wave D handler).
- POST `/wfb/pair` on the agent REST API (spec 11, Wave C Cellos routes).

Phase 1 POC: `wfb_keygen` (the upstream tool) generates fresh random
keypairs and does not accept an input seed. So the pair flow here is
"user supplied 32 bytes of shared key material, write those bytes
straight into rx.key and publish a public fingerprint." This is
acceptable for Phase 1 bench pairing. Phase 2 switches to a real
NaCl keypair exchange over the webapp QR path.

Key paths come from `ados.services.wfb.key_mgr.get_key_paths()` so air
and ground sides stay wire-compatible with a single source of truth.
"""

from __future__ import annotations

import asyncio
import hashlib
import os
import re
import subprocess
from datetime import UTC, datetime
from pathlib import Path

import yaml

from ados.core.logging import get_logger
from ados.services.wfb.key_mgr import get_key_paths

log = get_logger("ground_station.pair_manager")

_SETUP_COMPLETE_PATH = Path("/var/lib/ados/setup-complete")
_AP_PASSPHRASE_PATH = Path("/etc/ados/ap-passphrase")
_CONFIG_PATH = Path("/etc/ados/config.yaml")
_WFB_RX_UNIT = "ados-wfb-rx.service"

_HEX_RE = re.compile(r"^[0-9a-fA-F]+$")
_BASE58_RE = re.compile(r"^[1-9A-HJ-NP-Za-km-z]+$")

_MIN_KEY_LEN = 16
_WFB_KEY_BYTES = 32


def _iso_now() -> str:
    """Return the current UTC timestamp in ISO 8601 form."""
    return datetime.now(UTC).isoformat(timespec="seconds")


def _fingerprint(public_key_bytes: bytes) -> str:
    """Return the first 16 chars of the SHA-256 hex digest of the public key."""
    return hashlib.sha256(public_key_bytes).hexdigest()[:16]


def _decode_pair_key(pair_key: str) -> bytes:
    """Decode a pair_key string into raw 32-byte key material.

    Accepts 32-byte hex (64 chars), raw hex of any >= 16 char length
    (zero-padded or truncated to 32 bytes), or a best-effort UTF-8 byte
    expansion for non-hex inputs. Falls back to SHA-256 of the input
    bytes to always yield a stable 32-byte key. Phase 1 POC behavior.
    """
    if _HEX_RE.match(pair_key) and len(pair_key) % 2 == 0:
        try:
            raw = bytes.fromhex(pair_key)
            if len(raw) == _WFB_KEY_BYTES:
                return raw
            # Short or long hex: normalize via SHA-256 so we always get 32 bytes.
            return hashlib.sha256(raw).digest()
        except ValueError:
            pass

    # Non-hex or mixed input: derive deterministically via SHA-256. Also
    # handles base58 input without pulling in a base58 dep for the POC.
    return hashlib.sha256(pair_key.encode("utf-8")).digest()


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


def _load_config_dict() -> dict:
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


def _save_config_dict(data: dict) -> bool:
    """Atomically rewrite `/etc/ados/config.yaml` with the given dict."""
    try:
        body = yaml.safe_dump(data, sort_keys=False, default_flow_style=False)
        _atomic_write(_CONFIG_PATH, body.encode("utf-8"), mode=0o644)
        return True
    except (OSError, yaml.YAMLError) as exc:
        log.error("config_write_failed", path=str(_CONFIG_PATH), error=str(exc))
        return False


def _set_paired_drone_id(drone_id: str | None) -> str | None:
    """Persist `ground_station.paired_drone_id` to config.

    Returns the previous value, or None if unset. Passing None clears
    the field. Leaves the rest of the config untouched.
    """
    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if not isinstance(gs_section, dict):
        gs_section = {}
        data["ground_station"] = gs_section

    previous = gs_section.get("paired_drone_id")
    if drone_id is None:
        gs_section.pop("paired_drone_id", None)
    else:
        gs_section["paired_drone_id"] = drone_id

    _save_config_dict(data)
    return previous if isinstance(previous, str) else None


def _get_paired_drone_id() -> str | None:
    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if isinstance(gs_section, dict):
        drone_id = gs_section.get("paired_drone_id")
        if isinstance(drone_id, str) and drone_id:
            return drone_id
    return None


def _get_paired_at() -> str | None:
    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if isinstance(gs_section, dict):
        ts = gs_section.get("paired_at")
        if isinstance(ts, str) and ts:
            return ts
    return None


def _set_paired_at(ts: str | None) -> None:
    data = _load_config_dict()
    gs_section = data.get("ground_station")
    if not isinstance(gs_section, dict):
        gs_section = {}
        data["ground_station"] = gs_section
    if ts is None:
        gs_section.pop("paired_at", None)
    else:
        gs_section["paired_at"] = ts
    _save_config_dict(data)


def _systemctl(action: str, unit: str) -> bool:
    """Thin wrapper around `systemctl <action> <unit>`. Mirrors hostapd_manager."""
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
    """Raised when a pair_key fails validation."""


def _validate_pair_key(pair_key: str) -> None:
    """Reject blank, too-short, or obviously malformed pair keys.

    Accepts hex or base58 strings of at least 16 chars. Raises
    PairKeyError with a human-readable message on failure.
    """
    if not isinstance(pair_key, str):
        raise PairKeyError("pair_key must be a string")

    stripped = pair_key.strip()
    if not stripped:
        raise PairKeyError("pair_key is blank")

    if len(stripped) < _MIN_KEY_LEN:
        raise PairKeyError(
            f"pair_key too short: {len(stripped)} chars, minimum {_MIN_KEY_LEN}"
        )

    if not (_HEX_RE.match(stripped) or _BASE58_RE.match(stripped)):
        raise PairKeyError(
            "pair_key must be hex or base58 characters only"
        )


class PairManager:
    """Drone pair key exchange for the ground-station profile.

    Single instance per agent. Consumed by the Wave C REST routes
    (`POST /wfb/pair`) and by the Wave D long-press B3 handler. All
    operations are async for API symmetry even though the underlying
    file and subprocess work is synchronous.
    """

    def __init__(self, key_dir: str | None = None) -> None:
        tx_path, rx_path = get_key_paths(key_dir)
        # Ground-side RX key is the private half. The public half sits
        # alongside it so both the webapp and the paired drone can
        # reference a stable fingerprint.
        self._rx_key_path = Path(rx_path)
        self._rx_pub_path = Path(rx_path + ".pub")
        self._tx_key_path = Path(tx_path)

    @property
    def rx_key_path(self) -> Path:
        return self._rx_key_path

    @property
    def rx_pub_path(self) -> Path:
        return self._rx_pub_path

    def _current_fingerprint(self) -> str | None:
        """Return the fingerprint of the on-disk public key, if present."""
        if not self._rx_pub_path.is_file():
            return None
        try:
            pub_bytes = self._rx_pub_path.read_bytes()
            if not pub_bytes:
                return None
            return _fingerprint(pub_bytes)
        except OSError as exc:
            log.debug("pair_pub_read_failed", error=str(exc))
            return None

    async def pair(
        self,
        pair_key: str,
        drone_device_id: str | None = None,
    ) -> dict:
        """Install a shared pair key and mark the ground station as paired.

        Args:
            pair_key: Shared key material from the drone, minimum 16 hex
                or base58 chars. See the module docstring for the Phase
                1 POC semantics.
            drone_device_id: Optional device-id of the drone being
                paired. Persisted to agent config for UI display.

        Returns:
            Dict with keys: paired_drone_id, paired_at, key_fingerprint.

        Raises:
            PairKeyError: If the pair_key is blank, too short, or has
                invalid characters.
        """
        _validate_pair_key(pair_key)

        raw_key = _decode_pair_key(pair_key.strip())
        # Public half: SHA-256 of the private key. Not a real NaCl
        # public key, but stable and good enough as a fingerprint
        # anchor for Phase 1. Phase 2 replaces this with the real
        # libsodium `crypto_scalarmult_base` derivation.
        pub_bytes = hashlib.sha256(raw_key).digest()
        fingerprint = _fingerprint(pub_bytes)

        # Atomic write both files. Private key stays 0600, public key
        # can be 0644 so the webapp can read the fingerprint without
        # escalating.
        _atomic_write(self._rx_key_path, raw_key, mode=0o600)
        _atomic_write(self._rx_pub_path, pub_bytes, mode=0o644)

        paired_at = _iso_now()
        _set_paired_drone_id(drone_device_id or "unknown")
        _set_paired_at(paired_at)

        # Drop the setup-complete sentinel so captive_dns.py (Wave B)
        # stops redirecting. Best-effort: failure here does not unpair.
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

        # Reload wfb-rx so the running service picks up the new keys.
        # If the unit is not running yet, reload will fail; that is OK,
        # the next start reads the keys from disk anyway.
        reloaded = _systemctl("reload", _WFB_RX_UNIT)
        if not reloaded:
            log.info(
                "wfb_rx_reload_skipped",
                unit=_WFB_RX_UNIT,
                note="unit may not be active yet, keys will be picked up on next start",
            )

        log.info(
            "pair_complete",
            drone_device_id=drone_device_id or "unknown",
            key_fingerprint=fingerprint,
            paired_at=paired_at,
        )

        return {
            "paired_drone_id": drone_device_id or "unknown",
            "paired_at": paired_at,
            "key_fingerprint": fingerprint,
        }

    async def unpair(self) -> dict:
        """Remove the pair key and clear the paired_drone_id.

        Leaves `/var/lib/ados/setup-complete` in place so the device
        does not fall back into captive setup. The operator can run
        `factory_reset()` to wipe that too.
        """
        previous = _get_paired_drone_id()

        for path in (self._rx_key_path, self._rx_pub_path):
            try:
                if path.is_file():
                    path.unlink()
            except OSError as exc:
                log.warning(
                    "pair_key_delete_failed",
                    path=str(path),
                    error=str(exc),
                )

        _set_paired_drone_id(None)
        _set_paired_at(None)

        _systemctl("reload", _WFB_RX_UNIT)

        log.info("unpair_complete", previous_drone_id=previous)

        return {
            "unpaired": True,
            "previous_drone_id": previous,
        }

    async def status(self) -> dict:
        """Return live pair status.

        Fields: paired (bool), paired_drone_id, paired_at, key_fingerprint.
        """
        fingerprint = self._current_fingerprint()
        drone_id = _get_paired_drone_id()
        paired_at = _get_paired_at()
        paired = self._rx_key_path.is_file() and fingerprint is not None

        return {
            "paired": paired,
            "paired_drone_id": drone_id,
            "paired_at": paired_at,
            "key_fingerprint": fingerprint,
        }

    async def factory_reset(self) -> dict:
        """Wipe pair keys, setup sentinel, and AP passphrase.

        Forces the device back into first-boot posture: captive DNS
        responder will fire again, hostapd_manager will regenerate a
        fresh passphrase on next start, and the operator re-runs the
        setup webapp flow.
        """
        await self.unpair()

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

        ts = _iso_now()
        log.warning("factory_reset_performed", timestamp=ts)

        return {
            "reset": True,
            "timestamp": ts,
        }


# No systemd entry point. PairManager is consumed in-process by the
# REST API service (Wave C Cellos) and the physical UI handler (Wave D).
# A `__main__` block is intentionally omitted; for bench testing use
# `python -c "import asyncio; from ados.services.ground_station.pair_manager import PairManager; print(asyncio.run(PairManager().status()))"`.
