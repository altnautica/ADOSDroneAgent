"""Profile auto-detect for ADOS Drone Agent.

Runs a hardware fingerprint and always returns a usable profile (`drone`,
`ground_station` or `visor`). Each probe scores toward air (drone),
ground or visor; the decision tail is a strict argmax over the three
scores, with a stable tiebreaker on the last persisted profile (when it
is among the tied leaders) and a final `drone` default so a fresh-flashed
board with no signals still boots into a known state.

Visor is a rule, not a score: the ADOS Sensor Hub firmware on USB, or a
stock ArduPilot USB board next to two or more CSI cameras with no
long-range WFB adapter, makes the node a visor outright, and the flight
controller's own serial and heartbeat points do not count against it. An
ArduPilot board with cameras and a WFB adapter is a stereo drone. A GNSS
fix counts
toward air only when no OLED is present, since a handheld visor carries
both.

This module has no hard runtime dependencies beyond the stdlib. `smbus2`
and `pyserial` are used when available; otherwise the matching probes
silently contribute zero points.

Run a dry-run from a shell:

    python -m ados.bootstrap.profile_detect
"""

from __future__ import annotations

import json
import os
import socket
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:
    import structlog

    log = structlog.get_logger()
except ImportError:  # pragma: no cover - fall back to stdlib logging
    import logging

    log = logging.getLogger(__name__)

from ados.bootstrap._profile_detect_probes import (
    _DEFAULT_I2C_BUS,
    _FC_USB_VENDOR_IDS,
    _GPS_CANDIDATE_PATHS,
    _MAVLINK_SERIAL_PATHS,
    _NMEA_PREFIXES,
    _RTL8812_IDS,
    _UBX_SYNC,
    _config_claimed_serial_paths,
    _read_usb_vendor_for_tty,
    _serial_paths_held_open,
    board_button_pins,
    probe_fc_heartbeat,
    probe_gpio_buttons,
    probe_gps_serial,
    probe_i2c_oled,
    probe_mavlink_serial,
    probe_rtl8812,
)
from ados.bootstrap._profile_detect_uplink import (
    _ETHERNET_IFACE_PATTERNS,
    _detect_ethernet_iface,
    probe_uplink_kinds,
    probe_uplink_type,
)
from ados.bootstrap._profile_detect_visor import (
    HUB_ARDUPILOT_MULTICAM,
    HUB_FIRMWARE,
    probe_csi_cameras,
    probe_sensor_hub,
)
from ados.core._contracts_generated import PROFILES
from ados.core.paths import ADOS_RUN_DIR, PROFILE_CONF

__all__ = [
    "PROFILE_DETECT_JSON",
    "_DEFAULT_I2C_BUS",
    "_ETHERNET_IFACE_PATTERNS",
    "_FC_USB_VENDOR_IDS",
    "_GPS_CANDIDATE_PATHS",
    "_MAVLINK_SERIAL_PATHS",
    "_NMEA_PREFIXES",
    "_RTL8812_IDS",
    "_UBX_SYNC",
    "_config_claimed_serial_paths",
    "_detect_ethernet_iface",
    "_hostname_suggested_profile",
    "_main",
    "_now_iso",
    "_read_last_known_profile",
    "_read_usb_vendor_for_tty",
    "_serial_paths_held_open",
    "board_button_pins",
    "detect_profile",
    "probe_csi_cameras",
    "probe_fc_heartbeat",
    "probe_gps_serial",
    "probe_gpio_buttons",
    "probe_i2c_oled",
    "probe_mavlink_serial",
    "probe_rtl8812",
    "probe_sensor_hub",
    "probe_uplink_kinds",
    "probe_uplink_type",
    "read_detect_snapshot",
    "redetect",
    "write_detect_snapshot",
    "write_profile_conf",
]


# The last detection result, written at boot and on an explicit re-detect.
PROFILE_DETECT_JSON = ADOS_RUN_DIR / "profile-detect.json"

# Profiles the detector can pick, in default-preference order for ties.
_DETECTABLE = ("drone", "ground_station", "visor")


# ---- Decision -------------------------------------------------------------


def _hostname_suggested_profile() -> str | None:
    """Best-effort hostname → profile heuristic for the tiebreaker.

    Returns ``"drone"`` / ``"ground_station"`` / ``"visor"`` when the system hostname
    matches a well-known prefix, ``None`` otherwise. Mirrors (but is
    independent of) the same helper in ``ados.setup.profile`` — kept
    private here so the bootstrap module never imports the setup tree.
    """
    try:
        name = (socket.gethostname() or "").strip().lower()
    except OSError:
        return None
    if not name:
        return None
    if name.startswith(("groundstation", "gcs", "gs-")):
        return "ground_station"
    if name.startswith(("skynode", "drone", "rig-", "uav")):
        return "drone"
    if name.startswith("visor"):
        return "visor"
    return None


def detect_profile(config_override: str | None = None) -> dict[str, Any]:
    """Run all probes and decide the profile.

    Always returns a usable profile (`drone`, `ground_station` or
    `visor`). The decision tail is a strict argmax on the live probes,
    with a stable tiebreaker on the last persisted profile and a final
    `drone` default for a fresh-flashed board with no signals. The result
    includes a `source` field marking which branch of the decision
    produced the profile so the setup status can surface "auto" vs
    "needs review" cleanly.

    config_override: explicit value from /etc/ados/config.yaml. If it
    is a detectable profile that value is returned without running any
    probes.
    """
    if config_override in _DETECTABLE:
        return {
            "profile": config_override,
            "source": "override",
            "ground_score": 0,
            "air_score": 0,
            "visor_score": 0,
            "signals": {"override": config_override},
            "detected_at": _now_iso(),
        }

    # Probes are independent. The slow ones shell out to i2cdetect or
    # lsusb with a 3 s timeout; on a freshly hot-plugged USB bus those
    # calls sit waiting. Running them in a thread pool drops worst-case
    # detection time from roughly the sum of timeouts to roughly the
    # slowest single probe.
    probe_callables: dict[str, Callable[[], tuple[int, int, bool]]] = {
        "oled_i2c": probe_i2c_oled,
        "buttons_gpio": probe_gpio_buttons,
        "rtl8812": probe_rtl8812,
        "mavlink_serial": probe_mavlink_serial,
        "gps_serial": probe_gps_serial,
        "fc_heartbeat": probe_fc_heartbeat,
        "uplink": probe_uplink_type,
    }

    probes: dict[str, tuple[int, int, bool]] = {}
    with ThreadPoolExecutor(max_workers=len(probe_callables) + 1) as pool:
        futures = {name: pool.submit(fn) for name, fn in probe_callables.items()}
        hub_future = pool.submit(probe_sensor_hub)
        for name, future in futures.items():
            try:
                probes[name] = future.result()
            except Exception:
                probes[name] = (0, 0, False)
        try:
            hub_kind = hub_future.result()
        except Exception:
            hub_kind = ""
    hub_detected = bool(hub_kind)
    wfb_present = probes["rtl8812"][2]
    is_visor_rig = hub_kind == HUB_FIRMWARE or (
        hub_kind == HUB_ARDUPILOT_MULTICAM and not wfb_present
    )
    visor_score = 0
    if is_visor_rig:
        # The hub (or stage-A FC) is the visor's own sensor board: its MAVLink
        # serial port and heartbeat are not evidence of an aircraft.
        for name in ("mavlink_serial", "fc_heartbeat"):
            g, _a, d = probes[name]
            probes[name] = (g, 0, d)

    # A GNSS fix next to an OLED is a handheld visor, not evidence of a drone.
    if probes["oled_i2c"][2]:
        _g, _a, gps_detected = probes["gps_serial"]
        probes["gps_serial"] = (0, 0, gps_detected)

    ground_score = sum(g for g, _a, _d in probes.values())
    air_score = sum(a for _g, a, _d in probes.values())

    # Hostname is operator-controlled but easy to leave stale: a box
    # named `gs-field` six months ago might be wired up as a drone
    # today. Treat it as a soft tiebreaker rather than a hardware
    # override, so a real FC with a matched USB vendor (6 air points)
    # still wins over an inherited "gs-" hostname. Operators
    # who want hard pinning use `agent.profile` in /etc/ados/config.yaml
    # or `ados profile set`.
    hostname_pick = _hostname_suggested_profile()
    HOSTNAME_WEIGHT = 2
    hostname_source = ""
    if hostname_pick == "ground_station":
        ground_score += HOSTNAME_WEIGHT
        hostname_source = "hostname"
    elif hostname_pick == "drone":
        air_score += HOSTNAME_WEIGHT
        hostname_source = "hostname"
    elif hostname_pick == "visor":
        visor_score += HOSTNAME_WEIGHT
        hostname_source = "hostname"

    if is_visor_rig:
        visor_score = max(air_score, ground_score) + 1
    scores = {"drone": air_score, "ground_station": ground_score, "visor": visor_score}
    top = max(scores.values())
    leaders = [name for name in _DETECTABLE if scores[name] == top]
    if is_visor_rig:
        profile = "visor"
        source = "detected"
    elif len(leaders) == 1:
        profile = leaders[0]
        source = hostname_source or "detected"
    else:
        # Tied even after hostname weighting. Fall back to the last-known
        # persisted profile when it is one of the tied leaders, then the
        # safe default.
        prior = _read_last_known_profile()
        if prior in leaders:
            profile = prior
            source = "tiebreaker"
        else:
            profile = "drone"
            source = "default"

    signals = {name: detected for name, (_g, _a, detected) in probes.items()}
    signals["sensor_hub"] = hub_detected

    result = {
        "profile": profile,
        "source": source,
        "ground_score": ground_score,
        "air_score": air_score,
        "visor_score": visor_score,
        "signals": signals,
        "detected_at": _now_iso(),
    }
    try:
        log.debug(
            "profile_detect_result",
            profile=profile,
            source=source,
            ground_score=ground_score,
            air_score=air_score,
            visor_score=visor_score,
        )
    except Exception:
        pass
    return result


def write_detect_snapshot(result: dict[str, Any], path: Path = PROFILE_DETECT_JSON) -> bool:
    """Persist a detection result so readers never re-run the probes.

    The probes scan I2C, open UARTs and read the state socket, so they run
    once (at boot or on an explicit re-detect) and every status read serves
    this snapshot. Written atomically; a failure is reported, never raised.
    """
    target = Path(path)
    tmp = target.with_name(target.name + ".tmp")
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(result), encoding="utf-8")
        os.replace(tmp, target)
    except OSError:
        return False
    return True


def read_detect_snapshot(path: Path = PROFILE_DETECT_JSON) -> dict[str, Any] | None:
    """The persisted detection result, or ``None`` when absent or unreadable."""
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) and data.get("profile") else None


def redetect(path: Path = PROFILE_DETECT_JSON) -> dict[str, Any]:
    """Run every probe now and replace the persisted snapshot."""
    result = detect_profile(config_override=None)
    write_detect_snapshot(result, path)
    return result


def _now_iso() -> str:
    return (
        datetime.now(timezone.utc)
        .astimezone()
        .replace(microsecond=0)
        .isoformat()
    )


# ---- Persistence ----------------------------------------------------------


def _read_last_known_profile(path: str = str(PROFILE_CONF)) -> str | None:
    """Return the profile recorded in profile.conf, or None on any error.

    Used as a tiebreaker when the live probes are exactly tied. Failures
    (file missing, yaml unavailable, parse error) collapse to None so
    the caller falls through to the "drone" default and the operator can
    review the auto-pick from the dashboard.
    """
    target = Path(path)
    if not target.is_file():
        return None
    try:
        import yaml
    except ImportError:
        return None
    try:
        with open(target) as f:
            data = yaml.safe_load(f) or {}
    except (OSError, yaml.YAMLError):
        return None
    value = data.get("profile") if isinstance(data, dict) else None
    if value in _DETECTABLE:
        return value
    return None


def write_profile_conf(
    profile: str,
    path: str = str(PROFILE_CONF),
) -> bool:
    """Record a detected ``profile`` in /etc/ados/profile.conf.

    The file is co-owned: the installer writes ``profile``, ``channel`` and
    ``version`` there, and the upgrade path resolves the release channel
    from it. Only the ``profile`` key is set; every other key the file
    holds is kept. A file that already names a recognised profile is left
    untouched — detection only fills a gap, it never overrides a choice.

    Returns True when the file says ``profile`` afterwards. Failures are
    logged and swallowed because this runs early in boot when logging may
    not be wired yet.
    """
    if profile not in _DETECTABLE:
        return False
    try:
        import yaml  # local import so dry-run without pyyaml fails loudly
    except ImportError:
        try:
            log.error("profile_conf_write_failed", error="pyyaml not installed")
        except Exception:
            pass
        return False

    target = Path(path)
    try:
        existing: dict[str, Any] = {}
        if target.is_file():
            try:
                loaded = yaml.safe_load(target.read_text(encoding="utf-8"))
            except yaml.YAMLError:
                # Never replace a file this reader cannot parse: it may hold
                # the release channel in a shape only the installer knows.
                return False
            if isinstance(loaded, dict):
                existing = loaded
        current = existing.get("profile")
        if isinstance(current, str) and current.replace("-", "_") in PROFILES:
            return current.replace("-", "_") == profile
        existing["profile"] = profile
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.with_suffix(target.suffix + ".tmp")
        with open(tmp, "w") as f:
            yaml.safe_dump(existing, f, sort_keys=True)
        os.replace(tmp, target)
        try:
            log.info("profile_conf_written", path=str(target))
        except Exception:
            pass
        return True
    except OSError as exc:
        try:
            log.error(
                "profile_conf_write_failed",
                path=str(target),
                error=str(exc),
            )
        except Exception:
            pass
        return False


# ---- Dry-run entry point --------------------------------------------------


def _main() -> None:
    """Print the detection result as YAML. No file is written."""
    result = detect_profile(config_override=None)
    try:
        import yaml

        print(yaml.safe_dump(result, sort_keys=True).rstrip())
    except ImportError:
        import json

        print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    _main()
