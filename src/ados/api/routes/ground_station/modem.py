"""ModemManager-backed cellular signal endpoint.

Surfaces ``GET /api/v1/ground-station/modem-status`` for the LCD
uplink detail page and the future GCS Hardware sub-view.

Implementation notes:

* Shells out to ``mmcli`` (ModemManager CLI). When ``mmcli`` is not
  installed we return ``{present: false, reason: "modemmanager_not_installed"}``
  with HTTP 200 so the caller can surface a friendly message rather
  than handle a 5xx.
* Parses ``mmcli -L`` to detect modems and ``mmcli -m <idx> -K`` for
  the keyed (``modem.generic.signal-quality.value``) view that is
  easier to parse than the default human output.
* Reads the per-bearer view (``mmcli -b <idx> -K``) only when the
  generic view advertises a bearer, since asking for a non-existent
  bearer prints a noisy error.
* Caches the resolved snapshot for 5 seconds — modems do not change
  every second and signal probes are surprisingly slow on some
  RV1106 builds.
"""

from __future__ import annotations

import asyncio
import re
import time
from typing import Any

from fastapi import APIRouter

from ados.api.routes import ground_station as _gs

router = APIRouter(prefix="/v1/ground-station", tags=["ground-station"])

_CACHE_TTL_SECONDS = 5.0
_BEARER_PATH_RE = re.compile(r"/[\w/]+/Bearer/(\d+)")
_MODEM_PATH_RE = re.compile(r"/[\w/]+/Modem/(\d+)")


_cache_lock = asyncio.Lock()
_cache_value: dict[str, Any] | None = None
_cache_ts: float = 0.0


async def _which_mmcli() -> bool:
    """Return True when ``mmcli`` is available on PATH."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "which",
            "mmcli",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        out, _ = await proc.communicate()
    except FileNotFoundError:
        return False
    except Exception:  # noqa: BLE001
        return False
    return proc.returncode == 0 and bool(out.strip())


async def _run(cmd: list[str], timeout: float = 3.0) -> tuple[int, str]:
    """Run a subprocess and return ``(returncode, stdout_text)``.

    stderr is silently dropped; mmcli prints "couldn't find modem"
    style noise that we already convert into a structured response.
    """
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except FileNotFoundError:
        return 127, ""
    except Exception:  # noqa: BLE001
        return 1, ""
    try:
        out, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except TimeoutError:
        try:
            proc.kill()
        except Exception:  # noqa: BLE001
            pass
        return 124, ""
    return proc.returncode or 0, out.decode("utf-8", errors="replace")


def _parse_kv(text: str) -> dict[str, str]:
    """Parse ``mmcli -K`` output into a flat key:value dict.

    The keyed view emits one ``key : value`` per line. Empty values
    show up as ``--`` which we map to an empty string so callers do
    not have to special-case the literal.
    """
    out: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or ":" not in line:
            continue
        k, _, v = line.partition(":")
        k = k.strip()
        v = v.strip()
        if v == "--":
            v = ""
        out[k] = v
    return out


def _first_modem_index(text: str) -> int | None:
    for line in text.splitlines():
        m = _MODEM_PATH_RE.search(line)
        if m:
            try:
                return int(m.group(1))
            except ValueError:
                continue
    return None


def _first_bearer_index(generic: dict[str, str]) -> int | None:
    val = generic.get("modem.generic.bearers.value", "")
    if not val:
        return None
    m = _BEARER_PATH_RE.search(val)
    if m:
        try:
            return int(m.group(1))
        except ValueError:
            return None
    # Sometimes bearers come back as a comma-separated list; parse
    # the first numeric segment.
    for token in val.split(","):
        m = _BEARER_PATH_RE.search(token.strip())
        if m:
            try:
                return int(m.group(1))
            except ValueError:
                continue
    return None


def _to_float(value: str) -> float | None:
    if not value:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def _normalise_tech(value: str) -> str:
    v = value.lower()
    if "5g" in v:
        return "5gnr"
    if "lte" in v:
        return "lte"
    if "umts" in v or "hspa" in v:
        return "umts"
    if "gsm" in v or "edge" in v:
        return "gsm"
    return v or ""


async def _build_snapshot() -> dict[str, Any]:
    """Build the modem-status snapshot. Best-effort, never raises."""
    if not await _which_mmcli():
        return {"present": False, "reason": "modemmanager_not_installed"}

    rc, list_out = await _run(["mmcli", "-L"])
    if rc != 0:
        return {"present": False, "reason": "mmcli_list_failed"}
    if "no modems were found" in list_out.lower() or not list_out.strip():
        return {"present": False, "reason": "no_modem"}
    modem_idx = _first_modem_index(list_out)
    if modem_idx is None:
        return {"present": False, "reason": "no_modem"}

    rc, modem_out = await _run(["mmcli", "-m", str(modem_idx), "-K"])
    if rc != 0:
        return {"present": False, "reason": "mmcli_modem_failed"}
    generic = _parse_kv(modem_out)

    operator = generic.get("modem.3gpp.operator-name", "") or generic.get(
        "modem.generic.operator-name", ""
    )
    tech_raw = generic.get("modem.generic.access-technologies.value", "")
    tech = _normalise_tech(tech_raw)
    band = generic.get("modem.generic.bands.value", "") or generic.get(
        "modem.generic.current-bands.value", ""
    )
    rssi = _to_float(generic.get("modem.generic.signal-quality.value", ""))
    if rssi is not None:
        # mmcli reports signal-quality as a 0-100 percentage. We do
        # NOT translate it to dBm here because the percentage is its
        # own useful signal. RSSI dBm comes from --signal-get below.
        rssi_pct = rssi
    else:
        rssi_pct = None

    rc, signal_out = await _run(
        ["mmcli", "-m", str(modem_idx), "--signal-get", "-K"],
    )
    signal_kv = _parse_kv(signal_out) if rc == 0 else {}
    rsrp = _to_float(signal_kv.get("modem.signal.lte.rsrp", ""))
    rsrq = _to_float(signal_kv.get("modem.signal.lte.rsrq", ""))
    sinr = _to_float(signal_kv.get("modem.signal.lte.snr", "")) or _to_float(
        signal_kv.get("modem.signal.lte.sinr", "")
    )
    rssi_dbm = _to_float(signal_kv.get("modem.signal.lte.rssi", ""))
    if rssi_dbm is None:
        # 5G NR style key family.
        rsrp = rsrp or _to_float(signal_kv.get("modem.signal.5g.rsrp", ""))
        rsrq = rsrq or _to_float(signal_kv.get("modem.signal.5g.rsrq", ""))
        sinr = sinr or _to_float(signal_kv.get("modem.signal.5g.snr", ""))

    bearer_idx = _first_bearer_index(generic)
    ip: str = ""
    if bearer_idx is not None:
        rc, bearer_out = await _run(
            ["mmcli", "-b", str(bearer_idx), "-K"],
        )
        if rc == 0:
            bearer_kv = _parse_kv(bearer_out)
            ip = bearer_kv.get("bearer.ipv4-config.address", "") or bearer_kv.get(
                "bearer.ipv6-config.address", ""
            )

    return {
        "present": True,
        "operator": operator,
        "tech": tech,
        "band": band,
        "rssi_pct": rssi_pct,
        "rssi_dbm": rssi_dbm,
        "rsrp_dbm": rsrp,
        "rsrq_db": rsrq,
        "sinr_db": sinr,
        "ip": ip,
    }


async def _cached_snapshot() -> dict[str, Any]:
    global _cache_value, _cache_ts
    async with _cache_lock:
        now = time.monotonic()
        if _cache_value is not None and (now - _cache_ts) < _CACHE_TTL_SECONDS:
            return _cache_value
        snap = await _build_snapshot()
        _cache_value = snap
        _cache_ts = now
        return snap


@router.get("/modem-status")
async def get_modem_status() -> dict[str, Any]:
    """Cellular modem snapshot. Returns ``present: false`` when no modem."""
    _gs._require_ground_profile()
    return await _cached_snapshot()
