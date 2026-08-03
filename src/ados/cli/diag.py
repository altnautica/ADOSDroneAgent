"""``ados diag`` CLI subcommand tree — reliable, per-hop pipeline diagnosis.

Three operator-facing commands, all thin renderers over the agent's native REST
surface (the logic lives in the Rust control front, so the CLI shows exactly what
Mission Control sees):

* ``ados diag video`` — a per-hop verdict on the video pipeline (``/api/diag/video``).
  Each hop is judged by reliable cumulative/rate counters sampled over a window,
  NEVER by tcpdump / queue depth / a single snapshot, and the first real break is
  named as ``video dies at``.
* ``ados diag link`` — the WFB link diagnosis (``/api/wfb``): the one-glance
  ``link_diag`` verdict (deaf / mis_keyed / jammed / healthy / searching) plus the
  decode counters that separate the failure modes a bare "0 received" hides.
* ``ados diag storage`` — the storage-wear verdict (``/api/diag/storage``): the
  write counter read as a DELTA across the retained window, the throttle
  bitfield's sticky "has occurred" bits, and the store's own footprint including
  any quarantined corpse. The collector was already recording all of it; nothing
  read it back, so four cards were reflashed without anyone learning why.

Reach for these BEFORE hand-probing a pipeline (the reliable-diagnostics rule).
"""

from __future__ import annotations

import json
from typing import Any

import click
import httpx

from ados.cli import _ansi, api_bases
from ados.core.paths import PAIRING_JSON


def _load_api_key() -> str | None:
    try:
        if PAIRING_JSON.exists():
            data = json.loads(PAIRING_JSON.read_text(encoding="utf-8"))
            key = data.get("api_key")
            return key if isinstance(key, str) else None
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _auth_headers() -> dict[str, str]:
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _request(method: str, path: str, **kwargs: Any) -> dict[str, Any]:
    """Minimal REST helper. Returns the parsed body dict.

    Tries each candidate control port so a wrong-port probe never reads as
    "agent not running"; a connection refusal falls through to the next.
    """
    timeout = kwargs.pop("timeout", 10.0)
    try:
        for base in api_bases():
            try:
                with httpx.Client(timeout=timeout) as client:
                    resp = client.request(
                        method, f"{base}{path}", headers=_auth_headers(), **kwargs
                    )
                if resp.status_code >= 400:
                    raise click.ClickException(
                        f"Agent API returned {resp.status_code} for {path}"
                    )
                try:
                    body = resp.json()
                except ValueError as exc:
                    raise click.ClickException(
                        f"Agent API returned non-JSON for {path}"
                    ) from exc
                return body if isinstance(body, dict) else {"data": body}
            except httpx.ConnectError:
                continue  # this port refused; try the next candidate
        raise click.ClickException(
            "Agent is not running. Start the supervisor or run `ados demo`."
        )
    except httpx.HTTPError as exc:
        raise click.ClickException(str(exc)) from exc


# Map a video-hop verdict to a state dot + a plain colour for the summary line.
_HOP_STATE = {
    "flowing": "ok",
    "stalled": "fail",
    "no_upstream": "pending",
    "unknown": "warn",
}
# Map a storage-wear verdict to a state dot. "unknown" is deliberately a warning
# and not an "ok": a box whose store did not answer has not been proven healthy.
_WEAR_STATE = {
    "ok": "ok",
    "wearing": "warn",
    "critical": "fail",
    "unknown": "warn",
}
# Map a WFB link_diag verdict to a state dot.
_LINK_STATE = {
    "healthy": "ok",
    "searching": "pending",
    "deaf": "fail",
    "mis_keyed": "fail",
    "jammed": "warn",
}

# Packet loss at or above this is reported as lossy regardless of the decode
# verdict.
#
# `link_diag` answers "can this radio decode the peer at all" — deaf, mis-keyed,
# jammed, healthy. It is not a delivery verdict, and a link that decodes fine
# while dropping a fifth of the stream answers "healthy" to that question. For
# video that is not a healthy link, because a frame needs EVERY one of its
# packets: at roughly ten packets per frame, 5% packet loss already costs about
# 40% of frames and 20% costs about 90%. Losing frames at that rate is what an
# operator sees as a frozen picture, so the diagnostic says so instead of
# printing HEALTHY over it.
_LOSSY_LINK_PERCENT = 2.0

# Typical RTP packets per encoded video frame at the resolutions this link
# carries. Used only to explain the compounding to the operator, and always
# printed with the assumption stated — never as a measured figure.
_TYPICAL_PACKETS_PER_FRAME = 10


# How a loss figure is labelled by who took the measurement. A number whose
# provenance is invisible is a number an operator cannot act on: "3% loss" means
# something different when this node's own receiver counted it than when the
# ground station counted it on the other end of the downlink.
_MEASURED_BY = {
    "local": "(measured here)",
    "peer": "(measured by the receiving station)",
}


def link_loss(data: dict[str, Any]) -> tuple[float | None, str]:
    """Resolve the link's packet loss and which side measured it.

    A transmit-only drone has no local loss figure and never will: it injects
    its own downlink and a single radio in monitor mode cannot capture its own
    frames, so ``packets_received`` is a permanent zero and ``loss_percent`` is
    the matching sentinel. Gating the loss verdict on that counter made it dead
    code on the node type that most needs it.

    The measurement does exist — the receiving ground station counts exactly
    what arrived and sends it back up the link — so prefer whatever the radio
    resolved as its live sample (``measured_loss_percent`` alongside the
    ``sample_source`` naming who took it). Fall back to the local counters only
    when this node really did decode something itself.

    Returns ``(None, "none")`` when nobody measured, which is deliberately
    distinct from a measured zero.
    """
    source = data.get("sample_source")
    resolved = data.get("measured_loss_percent")
    if source in _MEASURED_BY and isinstance(resolved, (int, float)):
        return float(resolved), source

    # Older radio build with no resolved-sample fields: fall back to the local
    # counters, which are only meaningful once this node has decoded something.
    local = data.get("loss_percent")
    decoded = isinstance(data.get("packets_received"), (int, float)) and (
        data.get("packets_received") or 0
    ) > 0
    if decoded and isinstance(local, (int, float)):
        return float(local), "local"
    return None, "none"


def frame_delivery_percent(
    loss_percent: float, packets_per_frame: int = _TYPICAL_PACKETS_PER_FRAME
) -> float:
    """Estimate the share of frames arriving complete at a given packet loss.

    A frame is only usable if every packet carrying it arrives, so the loss
    compounds across the frame. This is an estimate under an independent-loss
    assumption; bursty loss is kinder to it. It exists to make the difference
    between "2% of packets" and "18% of frames" visible, because those two
    numbers describe the same link and only one of them matches what the
    operator is looking at.
    """
    if loss_percent <= 0.0:
        return 100.0
    if loss_percent >= 100.0:
        return 0.0
    survives = (1.0 - loss_percent / 100.0) ** packets_per_frame
    return survives * 100.0


@click.group("diag", help="Reliable per-hop pipeline diagnosis (video, link).")
def diag_group() -> None:
    pass


@diag_group.command("video", help="Per-hop verdict on the video pipeline.")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def diag_video(as_json: bool) -> None:
    data = _request("GET", "/api/diag/video")
    if as_json:
        click.echo(json.dumps(data, indent=2, sort_keys=True))
        return

    theme = _ansi.detect_theme()
    profile = data.get("profile", "?")
    role = data.get("role", "?")
    window = data.get("window_s")
    hops = data.get("hops") or []
    dies_at = data.get("video_dies_at")

    click.echo(_ansi.marker(theme, "VIDEO PIPELINE"))
    win = f"  ·  {window:.1f}s window" if isinstance(window, (int, float)) else ""
    click.echo(_ansi.kv(theme, "profile", f"{profile} / {role}{win}"))
    click.echo("")

    for hop in hops:
        verdict = str(hop.get("verdict", "unknown"))
        state = _HOP_STATE.get(verdict, "warn")
        label = str(hop.get("label", hop.get("name", "?")))
        metric = str(hop.get("metric", ""))
        method = str(hop.get("method", ""))
        detail = hop.get("detail")
        click.echo(
            f"  {_ansi.dot(theme, state)} {theme.bold(label)}  "
            f"{theme.dim(verdict.upper())}"
        )
        click.echo(_ansi.kv(theme, "", metric, label_width=2))
        if method:
            click.echo(_ansi.kv(theme, "", theme.dim(f"via {method}"), label_width=2))
        if isinstance(detail, str) and detail:
            click.echo(_ansi.kv(theme, "", theme.dim(detail), label_width=2))
        click.echo("")

    if isinstance(dies_at, str) and dies_at:
        click.echo(f"  {theme.fail(theme.glyph_fail())} video dies at: {theme.bold(dies_at)}")
    elif hops:
        click.echo(f"  {theme.ok(theme.glyph_ok())} all hops flowing")
    else:
        click.echo(f"  {theme.dim('no hops reported (pipeline not running?)')}")


@diag_group.command("link", help="WFB link diagnosis (deaf / mis_keyed / jammed / healthy).")
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def diag_link(as_json: bool) -> None:
    data = _request("GET", "/api/wfb")
    if as_json:
        click.echo(json.dumps(data, indent=2, sort_keys=True))
        return

    theme = _ansi.detect_theme()
    link_diag = data.get("link_diag")
    state = data.get("state", "?")

    loss, measured_by = link_loss(data)
    by = _MEASURED_BY.get(measured_by, "")
    lossy = loss is not None and loss >= _LOSSY_LINK_PERCENT
    # Whether this node's OWN receiver decoded anything. Distinct from having a
    # loss figure at all: a drone never decodes its own downlink, so this stays
    # false on one even while the ground station's measurement of that downlink
    # is right there in `loss`.
    measured = isinstance(data.get("packets_received"), (int, float)) and (
        data.get("packets_received") or 0
    ) > 0

    click.echo(_ansi.marker(theme, "WFB LINK"))
    if lossy:
        # A decode verdict of "healthy" is true and beside the point when a
        # fifth of the stream is missing. Lead with the delivery problem.
        dot = _ansi.dot(theme, "warn")
        click.echo(
            f"  {dot} {theme.bold('LOSSY')}  "
            f"{theme.dim(f'({loss:.1f}% packet loss {by}, decode={link_diag or state})')}"
        )
    elif isinstance(link_diag, str) and link_diag:
        # Name what each word is answering. The verdict describes what the radio
        # is DECODING right now; the state is the pairing lifecycle record. They
        # legitimately disagree -- a link decoding hundreds of packets a second
        # can sit behind a lifecycle record nothing has refreshed -- but printed
        # bare, "HEALTHY (state=stale)" reads as a surface contradicting itself,
        # and a reading an operator has to explain away is one they stop
        # trusting.
        dot = _ansi.dot(theme, _LINK_STATE.get(link_diag, "warn"))
        click.echo(
            f"  {dot} {theme.bold(link_diag.upper())} {theme.dim('decode')}"
            f"  {theme.dim(f'· pairing state: {state}')}"
        )
    else:
        # No verdict field on this reading — report the lifecycle state honestly
        # rather than inventing a verdict.
        click.echo(f"  {_ansi.dot(theme, 'pending')} {theme.bold(str(state).upper())}")
    click.echo("")

    def _num(key: str) -> str:
        v = data.get(key)
        return str(v) if isinstance(v, (int, float)) else "—"

    rssi = data.get("rssi_dbm")
    rssi_str = f"{rssi:.0f} dBm" if isinstance(rssi, (int, float)) else "— (no decode)"
    rows = [
        ("RSSI", rssi_str),
        ("Channel", _num("channel")),
        ("Decoded pkt/s", _num("packets_received")),
        ("RF frames (all)", _num("packets_all")),
        ("Decrypt errors", _num("decrypt_errors")),
        ("Bad packets", _num("packets_bad")),
        # These three decide whether video actually arrives, and were absent
        # from this readout entirely — so a link losing a quarter of the stream
        # printed as HEALTHY with nothing on screen to contradict it.
        ("Packets lost", _num("packets_lost")),
        ("FEC recovered", _num("fec_recovered")),
        ("FEC failed", _num("fec_failed")),
        (
            "Loss",
            f"{loss:.1f}% {by}" if loss is not None else "— (not measured)",
        ),
        ("Bitrate", f"{data.get('bitrate_mbps')} Mbps" if data.get("bitrate_mbps") else "—"),
    ]
    for label, value in rows:
        click.echo(_ansi.kv(theme, label, value, label_width=16))

    if lossy:
        delivered = frame_delivery_percent(float(loss))
        click.echo("")
        click.echo(
            theme.dim(
                f"  a frame needs every one of its packets, so at ~"
                f"{_TYPICAL_PACKETS_PER_FRAME} packets/frame this loss delivers "
                f"roughly {delivered:.0f}% of frames (estimate)."
            )
        )
    elif loss is None and not measured:
        # Nothing measured this link from either side. Say which measurements
        # are missing rather than printing a bare dash and leaving the operator
        # to guess whether the link is clean or unobserved.
        click.echo("")
        click.echo(
            theme.dim(
                "  no packet-loss measurement: this radio decoded nothing, and no "
                "receiving station has reported what it decoded from this node."
            )
        )


def _fmt_bytes(value: Any) -> str:
    """Human-readable size, or a dash when the number is genuinely absent."""
    if not isinstance(value, (int, float)):
        return "-"
    size = float(value)
    for unit in ("B", "KB", "MB", "GB"):
        if size < 1024:
            return f"{size:.0f} {unit}" if unit == "B" else f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} TB"


@diag_group.command(
    "storage",
    help="Storage-wear verdict (write rate, throttle history, store footprint).",
)
@click.option("--json", "as_json", is_flag=True, help="Output JSON for scripts.")
def diag_storage(as_json: bool) -> None:
    data = _request("GET", "/api/diag/storage")
    if as_json:
        click.echo(json.dumps(data, indent=2, sort_keys=True))
        return

    theme = _ansi.detect_theme()
    verdict = str(data.get("verdict", "unknown"))
    state = _WEAR_STATE.get(verdict, "warn")

    click.echo(_ansi.marker(theme, "STORAGE WEAR"))
    click.echo(f"  {_ansi.dot(theme, state)} {theme.bold(verdict.upper())}")
    reason = data.get("reason")
    if isinstance(reason, str) and reason:
        click.echo(_ansi.kv(theme, "", theme.dim(reason), label_width=2))
    click.echo("")

    write = data.get("write") or {}
    kb_s = write.get("kb_per_s")
    if isinstance(kb_s, (int, float)):
        detail = f"{kb_s:.1f} KB/s"
        gb_day = write.get("gb_per_day")
        if isinstance(gb_day, (int, float)):
            detail += f"  ·  {gb_day:.1f} GB/day"
        window = write.get("window_s")
        if isinstance(window, (int, float)):
            detail += f"  ·  {window:.0f}s window"
        click.echo(_ansi.kv(theme, "write rate", detail, label_width=16))
        device = write.get("device")
        if isinstance(device, str) and device:
            click.echo(_ansi.kv(theme, "device", device, label_width=16))
    else:
        # Never print a zero for an unmeasured rate — say why it is missing.
        why = write.get("reason") or "not measured"
        click.echo(
            _ansi.kv(theme, "write rate", theme.dim(f"- ({why})"), label_width=16)
        )

    throttle = data.get("throttle") or {}
    if throttle.get("supported") is True:
        if throttle.get("clean") is True:
            click.echo(
                _ansi.kv(theme, "power/thermal", "clean, no events recorded", 16)
            )
        else:
            flags = [
                ("undervoltage", throttle.get("undervoltage_occurred")),
                ("frequency capped", throttle.get("arm_frequency_capped_occurred")),
                ("throttled", throttle.get("throttling_occurred")),
                ("soft temp limit", throttle.get("soft_temperature_limit_occurred")),
            ]
            seen = [label for label, hit in flags if hit is True]
            click.echo(
                _ansi.kv(theme, "power/thermal", theme.fail(", ".join(seen) or "?"), 16)
            )
            click.echo(
                _ansi.kv(
                    theme,
                    "",
                    theme.dim("recorded at some point in the retained window"),
                    label_width=2,
                )
            )
    else:
        why = throttle.get("reason") or "not reported by this board"
        click.echo(_ansi.kv(theme, "power/thermal", theme.dim(f"- ({why})"), 16))

    store = data.get("store") or {}
    live = _fmt_bytes(store.get("live_bytes"))
    wal = _fmt_bytes(store.get("wal_bytes"))
    click.echo(_ansi.kv(theme, "log store", f"{live} live  ·  {wal} WAL", 16))
    quarantined = store.get("quarantined")
    if isinstance(quarantined, int) and quarantined > 0:
        corpse_bytes = _fmt_bytes(store.get("quarantined_bytes"))
        click.echo(
            _ansi.kv(
                theme,
                "quarantined",
                theme.fail(f"{quarantined} torn store(s), {corpse_bytes} not reclaimed"),
                16,
            )
        )

    fs = data.get("filesystem") or {}
    used_pct = fs.get("used_pct")
    if isinstance(used_pct, (int, float)):
        total = _fmt_bytes(fs.get("total_bytes"))
        click.echo(_ansi.kv(theme, "filesystem", f"{used_pct:.0f}% of {total} used", 16))
    else:
        why = fs.get("reason") or "not measured"
        click.echo(_ansi.kv(theme, "filesystem", theme.dim(f"- ({why})"), 16))
