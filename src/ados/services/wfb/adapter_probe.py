"""Cross-process probe for WFB adapter facts the radio service owns.

The drone-side WFB transmit service runs as its own systemd unit (a
compiled binary) and is the authoritative owner of the live adapter
scan, each interface's operating mode, and the regulatory-permitted
channel set. The REST surface, the bind interface setup, and the CLI
all live in *other* processes that need the same facts without poking
the radio's raw-socket internals.

This module is the thin, permanent read seam for those facts. Each
lookup resolves through three layers, fastest-and-freshest first:

1. The sidecar files the radio service writes atomically on every scan
   (``/run/ados/wfb-adapters.json`` for the full adapter list,
   ``/run/ados/hop-supervisor.json`` for the regulatory channel set).
   Used only while fresh so a dead service never serves stale facts.
2. The radio binary's one-shot ``adapters`` mode, which performs a live
   scan, prints the JSON list, and refreshes the sidecar. This is the
   path a pre-service caller (e.g. the bind setup before the radio unit
   is up) takes when no sidecar exists yet.
3. The in-process ``iw`` parse in :mod:`ados.services.wfb.adapter`, the
   permanent fallback for hosts where neither the sidecar nor the binary
   is present (the binary is absent on a Python-only build, and the
   sidecar is absent before the first scan).

Every function returns the exact shapes the existing Python callers
already consume, so importers keep working unchanged when the radio
moves to its own process.
"""

from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path

from ados.core.logging import get_logger
from ados.core.paths import HOP_SUPERVISOR_JSON, WFB_STATS_JSON

log = get_logger("wfb.adapter_probe")

# Full adapter-list sidecar written by the radio service on each scan.
# Lives on tmpfs (gone on reboot); the same shape the one-shot CLI prints.
WFB_ADAPTERS_JSON = WFB_STATS_JSON.parent / "wfb-adapters.json"

# The radio binary. ``adapters`` / ``--list-adapters`` performs a live
# scan, prints the JSON adapter list to stdout, and exits 0. Overridable
# for tests and non-default install layouts.
RADIO_BIN = os.environ.get("ADOS_RADIO_BIN", "/opt/ados/bin/ados-radio")

# A sidecar older than this is treated as not-present: a stopped radio
# service must never serve stale adapter facts as live. Generous because
# adapter topology changes are slow (a USB hot-plug, not a packet rate),
# and the writer refreshes it on every scan.
_SIDECAR_FRESH_S = 30.0

# Upper bound on the one-shot binary scan. A live ``iw`` sweep is a few
# hundred milliseconds; this guards against a wedged adapter or a hung
# ``iw`` child without stalling the caller indefinitely.
_BIN_TIMEOUT_S = 15.0


def _read_fresh_json(path: Path, max_age_s: float) -> object | None:
    """Return parsed JSON from ``path`` when it exists and is fresh.

    ``None`` on a missing file, a file older than ``max_age_s``, or any
    read / decode error. The freshness gate keys off mtime so a writer
    that stopped updating the file stops being trusted.
    """
    try:
        age_s = time.time() - path.stat().st_mtime
    except OSError:
        return None
    if age_s > max_age_s:
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _adapter_from_record(record: dict) -> object | None:
    """Build a :class:`WifiAdapterInfo` from one sidecar / CLI record.

    The radio service emits the same key names as the Python dataclass,
    but its nullable fields come across as JSON ``null`` where the Python
    dataclass uses non-null defaults: ``current_mode`` is ``""`` not
    ``None``, and ``usb_vid`` / ``usb_pid`` are ``0`` not ``None``. This
    normalises those back to the dataclass defaults so a consumer cannot
    tell which process produced the record.
    """
    from ados.services.wfb.adapter import WifiAdapterInfo

    name = record.get("interface_name")
    if not isinstance(name, str) or not name:
        return None

    def _as_int(value: object) -> int:
        return int(value) if isinstance(value, int) else 0

    caps = record.get("capabilities")
    capabilities = [c for c in caps if isinstance(c, str)] if isinstance(caps, list) else []

    return WifiAdapterInfo(
        interface_name=name,
        driver=str(record.get("driver") or ""),
        chipset=str(record.get("chipset") or ""),
        supports_monitor=bool(record.get("supports_monitor", False)),
        current_mode=str(record.get("current_mode") or ""),
        phy=str(record.get("phy") or ""),
        usb_vid=_as_int(record.get("usb_vid")),
        usb_pid=_as_int(record.get("usb_pid")),
        is_wfb_compatible=bool(record.get("is_wfb_compatible", False)),
        capabilities=capabilities,
    )


def _parse_adapter_list(payload: object) -> list | None:
    """Map a JSON adapter array into a list of ``WifiAdapterInfo``.

    Returns ``None`` when the payload is not a JSON array so the caller
    can fall through to the next probe layer. An array with some
    malformed entries yields the well-formed ones; a record missing an
    interface name is dropped.
    """
    if not isinstance(payload, list):
        return None
    out: list = []
    for record in payload:
        if not isinstance(record, dict):
            continue
        adapter = _adapter_from_record(record)
        if adapter is not None:
            out.append(adapter)
    return out


def _run_radio_adapters_cli() -> list | None:
    """Run the radio binary's one-shot adapter scan and parse its output.

    Returns the adapter list on success, ``None`` when the binary is
    missing, fails, times out, or prints output that is not a JSON array.
    The binary also refreshes ``/run/ados/wfb-adapters.json`` as a side
    effect, so the next call can take the faster sidecar path.
    """
    bin_path = Path(RADIO_BIN)
    if not bin_path.exists():
        return None
    try:
        result = subprocess.run(
            [str(bin_path), "adapters"],
            capture_output=True,
            text=True,
            timeout=_BIN_TIMEOUT_S,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError) as exc:
        log.debug("radio_adapters_cli_unavailable", error=str(exc))
        return None
    if result.returncode != 0:
        log.debug("radio_adapters_cli_failed", returncode=result.returncode)
        return None
    try:
        payload = json.loads(result.stdout)
    except ValueError:
        log.debug("radio_adapters_cli_bad_json")
        return None
    return _parse_adapter_list(payload)


def detect_wfb_adapters() -> list:
    """Return the detected WFB adapters, radio-service-owned facts first.

    Resolution order: the fresh ``wfb-adapters.json`` sidecar, then the
    radio binary's one-shot scan, then the in-process ``iw`` parse. The
    return type and per-adapter shape match the historical
    ``adapter.detect_wfb_adapters`` so every importer keeps working.
    """
    payload = _read_fresh_json(WFB_ADAPTERS_JSON, _SIDECAR_FRESH_S)
    parsed = _parse_adapter_list(payload) if payload is not None else None
    if parsed is not None:
        return parsed

    from_cli = _run_radio_adapters_cli()
    if from_cli is not None:
        return from_cli

    # Permanent fallback: the in-process scan. Imported lazily so this
    # module and ``adapter`` can re-export across each other without a
    # circular import at module load time.
    from ados.services.wfb.adapter import detect_wfb_adapters_iw

    return detect_wfb_adapters_iw()


def get_interface_mode(interface: str) -> str | None:
    """Return an interface's operating mode ("monitor" | "managed" | ...).

    Prefers the ``current_mode`` the radio service recorded for the
    interface in the fresh adapter sidecar, then falls back to the
    in-process ``iw <iface> info`` read. ``None`` when the mode cannot be
    determined by any layer.
    """
    if not interface:
        return None
    payload = _read_fresh_json(WFB_ADAPTERS_JSON, _SIDECAR_FRESH_S)
    if isinstance(payload, list):
        for record in payload:
            if not isinstance(record, dict):
                continue
            if record.get("interface_name") == interface:
                mode = record.get("current_mode")
                if isinstance(mode, str) and mode:
                    return mode
                break

    from ados.services.wfb.adapter import get_interface_mode_iw

    return get_interface_mode_iw(interface)


def enabled_channels(interface: str) -> set[int]:
    """5 GHz channels this adapter can use, regulatory-filtered.

    The radio service computes this set once per scan and mirrors it onto
    ``hop-supervisor.json``; that mirror is the cross-process source while
    the service runs. Falls back to the in-process ``iw phy <phy>
    channels`` parse otherwise (e.g. before the radio unit starts, when
    the bind interface setup needs the set). Empty set means "could not
    determine"; callers treat that as "do not restrict".
    """
    if not interface:
        return set()
    payload = _read_fresh_json(HOP_SUPERVISOR_JSON, _SIDECAR_FRESH_S)
    if isinstance(payload, dict):
        channels = payload.get("enabled_channels")
        if isinstance(channels, list):
            parsed = {int(c) for c in channels if isinstance(c, int)}
            if parsed:
                return parsed

    from ados.services.wfb.adapter import enabled_channels_iw

    return enabled_channels_iw(interface)


__all__ = [
    "detect_wfb_adapters",
    "enabled_channels",
    "get_interface_mode",
]
