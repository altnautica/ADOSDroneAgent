"""MAVLink v2 message signing helpers.

The agent does not own the signing key. MAVLink signing keys live in the GCS
browser as non-extractable Web Crypto CryptoKey objects. This module exposes
only the operations needed for one-shot FC enrollment, capability detection,
and the SIGNING_REQUIRE toggle.

A key reaches the agent exactly once, as raw bytes in the POST body of the
enroll-fc REST call. Those bytes are zeroized the moment SETUP_SIGNING has
been sent to the FC.
"""

from __future__ import annotations

import asyncio
import binascii
import hashlib
import time
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

from pymavlink import mavutil

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.services.mavlink.connection import FCConnection
    from ados.services.mavlink.param_cache import ParamCache
    from ados.services.mavlink.state import VehicleState

log = get_logger("mavlink.signing")

# 2015-01-01 00:00:00 UTC, expressed in seconds since POSIX epoch.
_EPOCH_2015 = datetime(2015, 1, 1, tzinfo=timezone.utc).timestamp()

# Minimum ArduPilot major+minor to expose signing params (ArduPilot 4.0+).
_MIN_ARDUPILOT_MAJOR = 4
_MIN_ARDUPILOT_MINOR = 0

# MAV_PARAM_TYPE_UINT8 = 1. SIGNING_REQUIRE is a uint8 on ArduPilot.
_PARAM_TYPE_UINT8 = 1


# ──────────────────────────────────────────────────────────────
# Capability detection
# ──────────────────────────────────────────────────────────────

def detect_capability(
    fc: FCConnection | None,
    vehicle_state: VehicleState | None,
    param_cache: ParamCache | None,
) -> dict[str, Any]:
    """Return whether the connected FC supports MAVLink signing.

    The check is strict on purpose: ArduPilot + version >= 4.0 + at least
    one SIGNING_* param in the cached param tree. Custom builds that
    stripped signing will correctly report unsupported.
    """
    if fc is None or not fc.connected:
        return {
            "supported": False,
            "reason": "fc_not_connected",
            "firmware_name": None,
            "firmware_version": None,
            "signing_params_present": False,
        }

    # MAV_AUTOPILOT enum — 3 = ArduPilot, 12 = PX4
    autopilot = vehicle_state.autopilot if vehicle_state else 0
    firmware_name = _autopilot_name(autopilot)

    if autopilot != mavutil.mavlink.MAV_AUTOPILOT_ARDUPILOTMEGA:
        # MSP firmwares never get a MAVLink HEARTBEAT, so autopilot==0 here
        # means no heartbeat received yet (already handled above by fc_not_connected).
        # Any other autopilot that isn't ArduPilot falls into this branch.
        if autopilot == mavutil.mavlink.MAV_AUTOPILOT_PX4:
            reason = "firmware_px4_no_persistent_store"
        elif autopilot == mavutil.mavlink.MAV_AUTOPILOT_INVALID:
            reason = "msp_protocol"
        else:
            reason = "firmware_not_supported"
        return {
            "supported": False,
            "reason": reason,
            "firmware_name": firmware_name,
            "firmware_version": None,
            "signing_params_present": False,
        }

    # Look for any SIGNING_* param in the cache. Presence is the strictest
    # gate because builds that stripped signing won't expose these params.
    signing_params_present = False
    if param_cache is not None:
        for name in param_cache.get_all().keys():
            if name.startswith("SIGNING_"):
                signing_params_present = True
                break

    # We can't derive major/minor from VehicleState.autopilot alone. The agent
    # doesn't currently cache AUTOPILOT_VERSION major/minor. If the SIGNING_*
    # params are present the firmware is clearly new enough; if they're absent
    # we assume the firmware is too old (or signing was stripped).
    if not signing_params_present:
        return {
            "supported": False,
            "reason": "firmware_too_old",
            "firmware_name": firmware_name,
            "firmware_version": None,
            "signing_params_present": False,
        }

    return {
        "supported": True,
        "reason": "ok",
        "firmware_name": firmware_name,
        "firmware_version": None,  # populated by GCS when it reads AUTOPILOT_VERSION
        "signing_params_present": True,
    }


def _autopilot_name(autopilot: int) -> str | None:
    if autopilot == 0:
        return None
    if autopilot == mavutil.mavlink.MAV_AUTOPILOT_ARDUPILOTMEGA:
        return "ArduPilot"
    if autopilot == mavutil.mavlink.MAV_AUTOPILOT_PX4:
        return "PX4"
    if autopilot == mavutil.mavlink.MAV_AUTOPILOT_INVALID:
        return "Unknown (non-MAVLink or heartbeat-only)"
    return f"Autopilot-{autopilot}"


# ──────────────────────────────────────────────────────────────
# FC enrollment (one-shot, key zeroized on return)
# ──────────────────────────────────────────────────────────────

def _initial_timestamp_10us() -> int:
    """Seconds since 2015-01-01 UTC, expressed in 10-microsecond units."""
    seconds_since_2015 = time.time() - _EPOCH_2015
    return int(seconds_since_2015 * 100_000)


def _fingerprint(key_bytes: bytes) -> str:
    """First 8 hex chars of SHA-256 over the key. Safe to display / log."""
    return hashlib.sha256(key_bytes).hexdigest()[:8]


async def enroll_fc(
    fc: FCConnection,
    key_bytes: bytearray,
    target_system: int = 1,
    target_component: int = 1,
) -> dict[str, Any]:
    """Send SETUP_SIGNING twice and zeroize the key buffer on exit.

    key_bytes MUST be a mutable bytearray so we can overwrite it in place.
    The caller must not reuse key_bytes after this function returns.

    Returns {success, key_id, enrolled_at}. key_id is the first 8 hex chars
    of sha256(key); the raw key never appears in the return value.
    """
    if len(key_bytes) != 32:
        raise ValueError(f"signing key must be 32 bytes, got {len(key_bytes)}")

    if not fc.connected or fc.connection is None:
        raise RuntimeError("FC not connected")

    conn = fc.connection
    initial_ts = _initial_timestamp_10us()
    key_id = _fingerprint(bytes(key_bytes))

    try:
        # pymavlink's setup_signing_send takes target_system, target_component,
        # secret_key (bytes), initial_timestamp. Sent twice 200ms apart to
        # survive a single-frame radio hiccup during enrollment.
        conn.mav.setup_signing_send(
            target_system,
            target_component,
            bytes(key_bytes),
            initial_ts,
        )
        await asyncio.sleep(0.2)
        conn.mav.setup_signing_send(
            target_system,
            target_component,
            bytes(key_bytes),
            initial_ts,
        )
    finally:
        # Zeroize the caller's buffer. Python memory is GC'd eventually, but
        # the bytearray is our caller's authoritative copy, so we overwrite
        # it explicitly. This runs even if setup_signing_send raised.
        for i in range(len(key_bytes)):
            key_bytes[i] = 0

    enrolled_at = datetime.now(tz=timezone.utc).isoformat(timespec="seconds")
    # Log the fingerprint, never the key. key_id is public-safe.
    log.info("signing_key_enrolled", key_id=key_id, target_system=target_system)
    return {
        "success": True,
        "key_id": key_id,
        "enrolled_at": enrolled_at,
    }


def disable_on_fc(
    fc: FCConnection,
    target_system: int = 1,
    target_component: int = 1,
) -> dict[str, Any]:
    """Clear the FC's signing store by sending 32 zero bytes + zero timestamp.

    ArduPilot recognises this as 'disable signing'.
    """
    if not fc.connected or fc.connection is None:
        raise RuntimeError("FC not connected")

    conn = fc.connection
    zero_key = bytes(32)
    conn.mav.setup_signing_send(target_system, target_component, zero_key, 0)
    log.info("signing_disabled_on_fc", target_system=target_system)
    return {"success": True}


def set_require(
    fc: FCConnection,
    require: bool,
    target_system: int = 1,
    target_component: int = 1,
) -> dict[str, Any]:
    """Write SIGNING_REQUIRE param (1 or 0) on the FC."""
    if not fc.connected or fc.connection is None:
        raise RuntimeError("FC not connected")

    conn = fc.connection
    conn.mav.param_set_send(
        target_system,
        target_component,
        b"SIGNING_REQUIRE",
        1.0 if require else 0.0,
        _PARAM_TYPE_UINT8,
    )
    log.info("signing_require_set", require=require)
    return {"success": True, "require": require}


def get_require(param_cache: ParamCache | None) -> dict[str, Any]:
    """Read SIGNING_REQUIRE from the param cache.

    Returns {require: bool | None}. None means the param hasn't been seen
    yet in the current session.
    """
    if param_cache is None:
        return {"require": None}
    value = param_cache.get("SIGNING_REQUIRE")
    if value is None:
        return {"require": None}
    return {"require": bool(int(value))}


# ──────────────────────────────────────────────────────────────
# Frame observer (counts signed frames, no crypto)
# ──────────────────────────────────────────────────────────────

class FrameObserver:
    """Counts MAVLink v2 frames with the signed bit set.

    The agent cannot validate signatures (it holds no key). This observer
    gives the GCS a sanity check that signed frames are actually transiting
    the IPC socket in both directions.
    """

    def __init__(self) -> None:
        self.tx_signed_count: int = 0
        self.rx_signed_count: int = 0
        self.last_signed_rx_at: float | None = None

    def observe_frame(self, frame: bytes, direction: str) -> None:
        """Peek at INC_FLAGS at offset 2 of a v2 frame."""
        if len(frame) < 10:
            return
        if frame[0] != 0xFD:
            return  # v1 frame, no signing possible
        inc_flags = frame[2]
        if inc_flags & 0x01:
            if direction == "tx":
                self.tx_signed_count += 1
            elif direction == "rx":
                self.rx_signed_count += 1
                self.last_signed_rx_at = time.time()

    def snapshot(self) -> dict[str, Any]:
        return {
            "tx_signed_count": self.tx_signed_count,
            "rx_signed_count": self.rx_signed_count,
            "last_signed_rx_at": self.last_signed_rx_at,
        }


# ──────────────────────────────────────────────────────────────
# Key-hex validation (used by the REST enroll-fc route)
# ──────────────────────────────────────────────────────────────

def parse_key_hex(key_hex: str) -> bytearray:
    """Parse a 64-char lowercase hex string into a 32-byte bytearray.

    Raises ValueError on any format error. Callers must treat the returned
    bytearray as sensitive and zeroize it after use.
    """
    if not isinstance(key_hex, str):
        raise ValueError("key_hex must be a string")
    if len(key_hex) != 64:
        raise ValueError(f"key_hex must be 64 hex chars, got {len(key_hex)}")
    try:
        raw = binascii.unhexlify(key_hex)
    except binascii.Error as exc:
        raise ValueError(f"key_hex is not valid hex: {exc}") from exc
    return bytearray(raw)
