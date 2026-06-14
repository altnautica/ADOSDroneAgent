"""Status and telemetry routes."""

from __future__ import annotations

import asyncio
import hashlib
import json
import subprocess
import time

from fastapi import APIRouter

from ados.core.paths import BOARD_JSON, MESH_STATE_JSON, PROFILE_CONF

router = APIRouter()

# Whether the detected board dict has been persisted to its sidecar this process.
# The board is static per boot, so the write-through runs once on the first status
# read that produces a non-empty board, not on every poll.
_board_persisted = False


def _persist_board_once(board_info: dict) -> None:
    """Persist the detected board dict to its sidecar once per process.

    A separate on-box reader (the native control surface) serves the full board
    block from this file rather than running HAL detect itself. Best-effort and
    atomic (temp sibling + rename), and a no-op for an empty board (a detect that
    raised) or once already written — a failed write never affects the response.
    """
    global _board_persisted
    if _board_persisted or not board_info:
        return
    try:
        BOARD_JSON.parent.mkdir(parents=True, exist_ok=True)
        tmp = BOARD_JSON.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(board_info, sort_keys=True))
        tmp.replace(BOARD_JSON)
        _board_persisted = True
    except OSError:
        pass


def _radio_to_camel(block: dict | None) -> dict | None:
    """Convert a snake_case radio block to the camelCase shape the GCS reads.

    ``build_radio_block`` emits snake_case (the cloud relay remaps it for
    the heartbeat path). The LAN-direct poll has no remapper, so convert
    here once so the consolidated status carries the same canonical shape
    Mission Control's radio normalizer expects.
    """
    if not block:
        return None
    out: dict = {}
    for key, value in block.items():
        head, *tail = key.split("_")
        out[head + "".join(p.title() for p in tail)] = value
    return out


def _gs_video_delivering(wfb_status: dict | None) -> bool:
    """True only when the ground-station WFB link is actually delivering video.

    A reachable WHEP endpoint is not proof of a live downlink: the
    mediamtx ground profile serves WHEP whether or not any frames are
    arriving over the radio, so probing WHEP alone reports "running" with
    zero inbound bytes. The trustworthy signal is the receive link state:
    the stats writer (ados-groundlink wfb_rx) marks ``state == "active"``
    once it is locked and decoding (it does not emit ``"connected"``), and
    ``valid_rx_packets_per_s`` / packet counters confirm frames are still
    flowing right now. Require both — a live link state AND a positive
    valid-decode rate — so a searching / reg-blocked / silent receiver is
    reported as not-delivering. (Operating rule 37: endpoint-reachable is
    never proof of data flowing.)
    """
    if not isinstance(wfb_status, dict):
        return False
    if wfb_status.get("state") not in ("active", "connected"):
        return False
    for key in ("valid_rx_packets_per_s", "packets_received"):
        val = wfb_status.get(key)
        if isinstance(val, (int, float)) and val > 0:
            return True
    return False


# ---------------------------------------------------------------------------
# Cache TTLs for the consolidated status payload.
#
# Keep these short enough that the GCS sees fresh data within a few poll
# cycles. Keep them long enough that the same poller does not refetch on
# every tick. The numbers below assume a 1 to 2 Hz GCS poll rate.
# ---------------------------------------------------------------------------

_VIDEO_DEPS_TTL = 30.0      # external binary lookups change only on package install
_PROFILE_CONF_TTL = 5.0     # profile.conf is rewritten by the operator only
_MESH_STATE_TTL = 1.0       # mesh-state.json is refreshed by the mesh service ~1Hz
_SERVICES_LIST_TTL = 5.0    # service transition snapshot is cheap to read but not free


async def _fetch_video_deps() -> dict[str, bool]:
    """Run the binary-presence scan in a worker thread."""
    from ados.core.deps import check_video_dependencies

    deps = await asyncio.to_thread(check_video_dependencies)
    return {d.name: d.found for d in deps}


async def _fetch_profile_conf() -> dict:
    """Read and parse /etc/ados/profile.conf into a dict."""
    def _read():
        try:
            import yaml as _yaml
            with open(PROFILE_CONF, encoding="utf-8") as fh:
                return _yaml.safe_load(fh) or {}
        except OSError:
            return {}

    return await asyncio.to_thread(_read)


async def _fetch_mesh_state() -> dict:
    """Read and parse /run/ados/mesh-state.json into a dict."""
    def _read():
        try:
            return json.loads(MESH_STATE_JSON.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}

    return await asyncio.to_thread(_read)


def _build_services_list_sync(app) -> list[dict]:
    """Synchronous services list builder. Pure data, no I/O."""
    tracker = app.service_tracker
    tracker_data = tracker.to_dict()
    services: list[dict] = []
    tasks = app.service_tasks()
    task_names = {t.get_name() for t in tasks}
    now_mono = time.monotonic()

    for task_item in tasks:
        name = task_item.get_name()
        tracked = tracker_data.get(name, {})
        raw_state = tracked.get("state", "running" if not task_item.done() else "stopped")
        svc_entry = {
            "name": name,
            "state": raw_state,
            "task_done": task_item.done(),
        }
        transitions = tracker.get_transitions(name)
        svc_uptime = 0.0
        if transitions:
            for ts, st in reversed(transitions):
                if st.value == "running":
                    svc_uptime = now_mono - ts
                    break
        svc_entry["uptimeSeconds"] = round(svc_uptime)
        services.append(svc_entry)

    for name, info in tracker_data.items():
        if name not in task_names:
            svc_entry = {
                "name": name,
                "state": info["state"],
                "task_done": True,
            }
            transitions = tracker.get_transitions(name)
            svc_uptime = 0.0
            if transitions:
                for ts, st in reversed(transitions):
                    if st.value == "running":
                        svc_uptime = now_mono - ts
                        break
            svc_entry["uptimeSeconds"] = round(svc_uptime)
            services.append(svc_entry)

    if not services:
        services = _systemd_services_fallback()

    # Per-service memory from cgroup accounting. Probed once per distinct
    # owning unit; entries with no unit or a stopped unit land at 0.0.
    # Memoized along with the rest of this list by the 5s status cache.
    _attach_service_memory(services)

    return services


def _attach_service_memory(services: list[dict]) -> None:
    """Add a ``memory_mb`` field to each service entry, in place.

    Mirrors the ``/api/services`` route: resolve each entry's systemd
    unit, read ``MemoryCurrent`` once per distinct unit, write the MiB
    value back. Best-effort and never raises.
    """
    from ados.core.systemd_memory import services_memory_mb, unit_for_service

    unit_by_entry: list[str | None] = [
        unit_for_service(s.get("name", "")) for s in services
    ]
    distinct_units = sorted({u for u in unit_by_entry if u})
    by_unit = services_memory_mb(distinct_units) if distinct_units else {}
    for svc, unit in zip(services, unit_by_entry):
        svc["memory_mb"] = by_unit.get(unit, 0.0) if unit else 0.0


def _systemd_services_fallback() -> list[dict]:
    """Enumerate ados-* systemd units when the in-process tracker is empty.

    The standalone API service runs in its own process under the
    multi-process supervisor. The in-process ServiceTracker only sees
    services managed inside the same Python process, so on that path
    the tracker is empty and the consolidated status reports
    ``services: []`` even when every ``ados-*.service`` is active.

    This fallback shells ``systemctl list-units 'ados-*.service'`` once
    per cache window so the GCS sees the real fleet of running units.
    """
    import shutil

    if not shutil.which("systemctl"):
        return []

    try:
        result = subprocess.run(
            [
                "systemctl",
                "list-units",
                "--type=service",
                "--all",
                "--no-pager",
                "--no-legend",
                "ados-*.service",
            ],
            capture_output=True,
            text=True,
            timeout=3,
        )
    except (OSError, subprocess.SubprocessError):
        return []

    if result.returncode != 0:
        return []

    services: list[dict] = []
    for line in result.stdout.splitlines():
        parts = line.split(None, 4)
        if len(parts) < 4:
            continue
        unit = parts[0].lstrip("●*").strip()
        sub = parts[3].strip()
        if not unit.endswith(".service"):
            continue
        name = unit[: -len(".service")]
        state = "running" if sub == "running" else sub or "unknown"
        services.append(
            {
                "name": name,
                "state": state,
                "status": state,
                "task_done": state != "running",
                "uptimeSeconds": 0,
            }
        )
    return services


def _compute_etag(payload: dict) -> str:
    """Stable short ETag over a JSON-serializable payload."""
    raw = json.dumps(payload, sort_keys=True, default=str).encode("utf-8")
    return hashlib.sha256(raw).hexdigest()[:16]


def _read_camera_status() -> dict[str, object]:
    """Camera presence + USB-recovery state for the LOCAL status surface.

    The cloud heartbeat carries ``cameraState``; the LAN-direct status did not,
    so a locally-paired operator got no camera-missing signal. Reads the same two
    sidecars the video pipeline + supervisor write, staleness-gated, and returns
    only the keys that are fresh and valid (absent otherwise).
    """
    from ados.core.paths import CAMERA_STATE_JSON, CAMERA_USB_RECOVERY_JSON

    out: dict[str, object] = {}
    try:
        camera = json.loads(CAMERA_STATE_JSON.read_text())
    except (OSError, ValueError):
        camera = None
    if isinstance(camera, dict):
        updated = camera.get("updated_at_unix")
        fresh = isinstance(updated, (int, float)) and (
            time.time() - float(updated)
        ) <= 300.0
        state = camera.get("state")
        if fresh and isinstance(state, str) and state in ("ready", "missing", "error"):
            out["cameraState"] = state

    try:
        rec = json.loads(CAMERA_USB_RECOVERY_JSON.read_text())
    except (OSError, ValueError):
        rec = None
    if isinstance(rec, dict):
        updated = rec.get("updated_at_unix")
        fresh = isinstance(updated, (int, float)) and (
            time.time() - float(updated)
        ) <= 60.0
        state = rec.get("camera_usb_recovery_state")
        if fresh and state in (
            "idle",
            "monitoring",
            "rebinding",
            "port_cycling",
            "hub_resetting",
            "needs_hub_reset",
            "guard_blocked",
            "exhausted",
        ):
            out["cameraUsbRecovery"] = {
                "state": state,
                "case": rec.get("case"),
                "attempts": rec.get("attempts", 0),
                "maxAttempts": rec.get("max_attempts", 0),
                "cameraPresent": bool(rec.get("camera_present", False)),
                "expected": bool(rec.get("expected", False)),
                "pppsCapable": bool(rec.get("ppps_capable", False)),
            }
    return out

