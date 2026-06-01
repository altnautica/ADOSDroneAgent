"""Client for the native transmit plane's operator command socket.

The data plane's FEC ratio, MCS index, TX power, and the auto/manual link
tier are operator knobs. When the native radio (``ados-radio``) is the
running transmit plane, the REST layer has no in-process Python manager to
call, so it forwards each knob change to the radio's command socket at
``/run/ados/wfb-cmd.sock`` instead. The running service applies it to the
live transmit process group (and the adaptive controller, for the tier
toggle).

Wire protocol (same framing as the supervisor bind socket): one
newline-terminated JSON request, one newline-terminated JSON response per
connection, then the server closes.

    {"op": "set_tx_power", "tx_power_dbm": 10}
        -> {"ok": true, "effective_dbm": 10}
    {"op": "set_fec", "fec_k": 8, "fec_n": 12}
        -> {"ok": true, "fec_k": 8, "fec_n": 12}
    {"op": "set_mcs", "mcs_index": 3}
        -> {"ok": true, "mcs_index": 3}
    {"op": "set_tier", "mode": "auto"}
        -> {"ok": true, "mode": "auto", "adaptive_bitrate_enabled": true}
    {"op": "set_tier", "mode": "manual",
     "mcs_index": 3, "fec_k": 8, "fec_n": 10}
        -> {"ok": true, "mode": "manual", ...}

A reply with ``ok: false`` carries an ``error`` code; the helpers raise
``RadioCmdError`` so the REST layer surfaces it. An unreachable socket
(the native radio is not up yet, or the box is in the packaged path)
raises ``RadioCmdUnavailableError`` so the caller can decide whether to
fall back to the packaged manager.
"""

from __future__ import annotations

import asyncio
import json

from ados.core.logging import get_logger
from ados.core.paths import WFB_CMD_SOCK

log = get_logger("wfb.cmd_client")

# Connecting to a live socket is local and near-instant; a short cap keeps
# a missing/refused socket from stalling the request.
_CONNECT_TIMEOUT_S = 2.0
# The apply can restart the data plane (an FEC/MCS/manual-tier change forks
# a fresh wfb_tx), so the reply can take a little longer than a pure read.
_REPLY_TIMEOUT_S = 10.0


class RadioCmdError(RuntimeError):
    """The command socket reported a failed apply (``ok: false``)."""


class RadioCmdUnavailableError(RuntimeError):
    """The command socket could not be reached.

    The native radio is not serving the socket (it is not up yet, or the
    box is running the packaged transmit plane). The caller decides
    whether to fall back to the packaged manager.
    """


async def _roundtrip(req: dict) -> dict:
    """Send one request, read one reply. Raises on an unreachable socket or
    a server-reported failure."""
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(str(WFB_CMD_SOCK)),
            timeout=_CONNECT_TIMEOUT_S,
        )
    except (TimeoutError, OSError) as exc:
        raise RadioCmdUnavailableError(
            f"radio command socket unavailable at {WFB_CMD_SOCK}"
        ) from exc

    try:
        writer.write((json.dumps(req) + "\n").encode("utf-8"))
        await writer.drain()
        try:
            line = await asyncio.wait_for(
                reader.readline(), timeout=_REPLY_TIMEOUT_S
            )
        except (TimeoutError, OSError) as exc:
            raise RadioCmdUnavailableError(
                "radio command socket did not reply in time"
            ) from exc
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except Exception:  # noqa: BLE001 — teardown is best-effort
            pass

    if not line:
        raise RadioCmdUnavailableError(
            "radio command socket closed connection before replying"
        )
    resp = json.loads(line.decode("utf-8"))
    if resp.get("ok") is False:
        raise RadioCmdError(resp.get("error") or "unknown radio command error")
    return resp


async def set_tx_power(dbm: int) -> int | None:
    """Apply a TX power (dBm) to the native radio. Returns the effective dBm
    the driver accepted (it can ramp UP from a rejected low request), or
    None when every ramp step was rejected."""
    resp = await _roundtrip({"op": "set_tx_power", "tx_power_dbm": int(dbm)})
    eff = resp.get("effective_dbm")
    return int(eff) if isinstance(eff, (int, float)) else None


async def set_fec(fec_k: int, fec_n: int) -> None:
    """Apply a Reed-Solomon ``(k, n)`` ratio to the native data plane."""
    await _roundtrip({"op": "set_fec", "fec_k": int(fec_k), "fec_n": int(fec_n)})


async def set_mcs(mcs_index: int) -> None:
    """Apply an MCS index to the native data plane."""
    await _roundtrip({"op": "set_mcs", "mcs_index": int(mcs_index)})


async def set_tier_auto() -> None:
    """Re-arm the adaptive controller (auto link tier)."""
    await _roundtrip({"op": "set_tier", "mode": "auto"})


async def set_tier_manual(mcs_index: int, fec_k: int, fec_n: int) -> None:
    """Pin the operator's ``(mcs, fec_k, fec_n)`` trio (manual link tier)."""
    await _roundtrip(
        {
            "op": "set_tier",
            "mode": "manual",
            "mcs_index": int(mcs_index),
            "fec_k": int(fec_k),
            "fec_n": int(fec_n),
        }
    )
