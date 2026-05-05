"""Universal setup and onboarding API routes."""

from __future__ import annotations

import asyncio
import shutil
from contextlib import suppress
from typing import Literal

import httpx
from fastapi import APIRouter, HTTPException, Request, WebSocket, WebSocketDisconnect
from pydantic import BaseModel, Field

from ados.api.deps import get_agent_app
from ados.core.logging import get_logger
from ados.core.paths import DISPLAY_CONF_PATH
from ados.hal.detect import _load_board_profiles, detect_board
from ados.setup import display_install, state as setup_state
from ados.setup.hardware_check import run_hardware_check
from ados.setup.models import (
    DisplayInstallRequest,
    DisplayJob,
    DisplayOption,
    DisplayOptionsResponse,
    HardwareCheckStatus,
    SetupActionResult,
    SetupStatus,
)
from ados.setup.profile import apply_profile
from ados.setup.service import (
    apply_cloud_choice,
    build_setup_status,
    install_cloudflare_token,
)

router = APIRouter(prefix="/v1/setup", tags=["setup"])

log = get_logger("setup_api")

# Canonical step ids the wizard emits. Used to validate skip targets so
# operators cannot stash arbitrary keys in the state file.
_VALID_STEP_IDS: frozenset[str] = frozenset(
    {
        "welcome",
        "profile",
        "hardware_check",
        "cloud_choice",
        "pair",
        "mavlink",
        "video",
        "ground_receiver",
        "display",
        "remote_access",
        "finish",
    }
)


class CloudflareTokenRequest(BaseModel):
    token_or_script: str


class SelfHostedBackendRequest(BaseModel):
    url: str
    mqtt_broker: str = ""
    mqtt_port: int = 8883
    api_key: str = ""


class CloudChoiceRequest(BaseModel):
    mode: Literal["cloud", "self_hosted", "local"]
    self_hosted: SelfHostedBackendRequest | None = Field(default=None)


class ProfileChoiceRequest(BaseModel):
    profile: Literal["drone", "ground_station"]
    ground_role: Literal["direct", "relay", "receiver"] | None = Field(default=None)


@router.get("/status", response_model=SetupStatus)
async def get_setup_status(request: Request) -> SetupStatus:
    """Return the universal setup state consumed by web, CLI, and GCS clients."""
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )


@router.post("/remote-access/cloudflare", response_model=SetupActionResult)
async def configure_cloudflare_tunnel(request: CloudflareTokenRequest) -> SetupActionResult:
    """Install a remotely managed Cloudflare Tunnel token or install command."""
    return install_cloudflare_token(get_agent_app(), request.token_or_script)


@router.post("/profile", response_model=SetupActionResult)
async def configure_profile(request: ProfileChoiceRequest) -> SetupActionResult:
    """Persist the operator's profile choice from the onboarding wizard.

    ``ground_role`` is required when ``profile`` is ``ground_station``
    and selects the distributed-RX role on the ground station node.
    """
    return apply_profile(
        get_agent_app(),
        profile=request.profile,
        ground_role=request.ground_role,
    )


@router.get("/hardware-check", response_model=HardwareCheckStatus)
async def get_hardware_check() -> HardwareCheckStatus:
    """Return the per-component hardware readiness snapshot for the active profile."""
    runtime = get_agent_app()
    config = runtime.config
    profile = str(config.agent.profile)
    if profile == "auto":
        profile = "drone"
    role = str(getattr(config.ground_station, "role", "direct") or "direct")
    return run_hardware_check(runtime, profile=profile, ground_role=role)


@router.post("/hardware-check/refresh", response_model=HardwareCheckStatus)
async def refresh_hardware_check() -> HardwareCheckStatus:
    """Re-run the hardware sweep on demand (no caching).

    Wired so the wizard can offer a Refresh button after the operator
    hot-plugs a USB device or swaps a camera mid-onboarding.
    """
    runtime = get_agent_app()
    config = runtime.config
    profile = str(config.agent.profile)
    if profile == "auto":
        profile = "drone"
    role = str(getattr(config.ground_station, "role", "direct") or "direct")
    return run_hardware_check(runtime, profile=profile, ground_role=role)


@router.get("/display/options", response_model=DisplayOptionsResponse)
async def get_display_options() -> DisplayOptionsResponse:
    """Return the supported displays for the active board plus the current state.

    Reads ``displays.supported`` from the active board's YAML profile via
    the HAL. Always includes a synthetic ``{ id: "none" }`` option so the
    wizard can offer an explicit skip.
    """
    board = detect_board()
    board_id = board.name or ""
    options: list[DisplayOption] = []

    # Walk the loaded board profiles and find the one whose ``name``
    # matches the running board. The HAL's BoardProfile carries the
    # rich ``displays`` block; we project it onto the wizard's option
    # shape and let the SPA render the picker.
    for profile in _load_board_profiles():
        if profile.name != board.name:
            continue
        for binding in profile.displays.supported:
            options.append(
                DisplayOption(
                    id=binding.id,
                    label=_label_for_display(binding.id, binding.controller),
                    controller=binding.controller,
                    touch_chip=binding.touch_chip,
                    resolution=binding.resolution,
                )
            )
        break

    options.append(
        DisplayOption(id="none", label="Skip / no display attached")
    )

    current = _read_display_conf()
    return DisplayOptionsResponse(
        board_id=board_id,
        current=current,
        supported=options,
    )


@router.post("/display/install", response_model=DisplayJob)
async def trigger_display_install(request: DisplayInstallRequest) -> DisplayJob:
    """Spawn the LCD-overlay installer (or write the skip marker).

    ``display_id="none"`` is the skip path: the route writes
    ``/etc/ados/display.conf`` with ``display_id=none`` and returns a
    synthetic ``done`` job so the wizard can flip the step to
    ``optional`` without polling. Any other id spawns the shell driver
    via the in-process job tracker. Concurrent install requests get a
    409 ``Conflict``.
    """
    if not request.display_id:
        raise HTTPException(status_code=400, detail="display_id is required")

    if request.display_id == "none":
        try:
            display_install.write_skip_marker()
        except PermissionError as exc:
            raise HTTPException(
                status_code=500,
                detail=(
                    "Cannot write /etc/ados/display.conf — agent must run "
                    f"with permission to write the config dir ({exc})."
                ),
            ) from exc
        return DisplayJob(
            job_id="skip",
            display_id="none",
            status="done",
            started_at=_now_iso(),
            finished_at=_now_iso(),
            exit_code=0,
            log_tail=["operator skipped the display step"],
        )

    try:
        handle = await display_install.start_install(request.display_id)
    except RuntimeError as exc:
        raise HTTPException(status_code=409, detail=str(exc)) from exc
    except FileNotFoundError as exc:
        raise HTTPException(status_code=500, detail=str(exc)) from exc

    return DisplayJob(**handle.to_dict())


@router.get("/display/job/{job_id}", response_model=DisplayJob)
async def get_display_install_job(job_id: str) -> DisplayJob:
    """Poll the status of an in-flight or completed install job.

    The wizard hits this at 1-2 Hz while the job is queued or running
    and renders the ``log_tail`` so the operator can watch progress.
    Synthetic ``skip`` jobs (from the ``display_id=none`` path) return
    a static ``done`` snapshot.
    """
    if job_id == "skip":
        return DisplayJob(
            job_id="skip",
            display_id="none",
            status="done",
            started_at=_now_iso(),
            finished_at=_now_iso(),
            exit_code=0,
            log_tail=["operator skipped the display step"],
        )
    handle = display_install.get_job(job_id)
    if handle is None:
        raise HTTPException(status_code=404, detail=f"Unknown job id: {job_id}")
    return DisplayJob(**handle.to_dict())


def _label_for_display(display_id: str, controller: str) -> str:
    """Operator-facing label for a supported display id."""
    known = {
        "waveshare35a": 'Waveshare 3.5" SPI LCD',
        "waveshare35b": 'Waveshare 3.5" SPI LCD (B)',
        "waveshare35c": 'Waveshare 3.5" SPI LCD (C)',
    }
    return known.get(display_id, f"{display_id} ({controller})")


def _read_display_conf() -> dict[str, str] | None:
    """Parse /etc/ados/display.conf into a plain dict, or None when absent."""
    if not DISPLAY_CONF_PATH.exists():
        return None
    out: dict[str, str] = {}
    try:
        for raw in DISPLAY_CONF_PATH.read_text().splitlines():
            line = raw.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, _, v = line.partition("=")
            out[k.strip()] = v.strip()
    except OSError:
        return None
    return out or None


def _now_iso() -> str:
    from datetime import datetime, timezone

    return (
        datetime.now(timezone.utc).astimezone().replace(microsecond=0).isoformat()
    )


@router.post("/reboot", response_model=SetupActionResult)
async def trigger_reboot() -> SetupActionResult:
    """Reboot the agent host on a short delay so the response delivers first.

    Wired so the wizard's display step can follow a successful overlay
    install with a single click. The 3-second delay is enough for the
    HTTP response to make it back to the browser before systemd-shutdown
    closes the socket; the wizard then polls /v1/setup/status until the
    agent comes back online.
    """
    asyncio.create_task(_reboot_after_delay(3.0))
    log.info("reboot_scheduled", delay_seconds=3)
    return SetupActionResult(
        ok=True,
        message="Reboot scheduled in 3 seconds. The wizard will reconnect automatically.",
    )


async def _reboot_after_delay(seconds: float) -> None:
    """Sleep then issue the reboot. Tries systemctl first, falls back to /sbin/reboot."""
    await asyncio.sleep(seconds)
    candidates: list[list[str]] = [
        ["systemctl", "reboot"],
        ["/sbin/reboot"],
        ["reboot"],
    ]
    for cmd in candidates:
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.wait()
            return
        except FileNotFoundError:
            continue
        except Exception as exc:  # noqa: BLE001
            log.warning("reboot_command_failed", cmd=cmd, error=str(exc))
    log.error("reboot_all_commands_failed")


@router.post("/cloud-choice", response_model=SetupActionResult)
async def configure_cloud_choice(request: CloudChoiceRequest) -> SetupActionResult:
    """Set the agent's cloud posture (cloud / self_hosted / local).

    Local mode disables the cloud relay entirely. Self-hosted mode records
    the operator's Convex + MQTT coordinates and writes any provided API
    key to a root-owned secret file. The API key is never echoed back.
    """
    self_hosted = request.self_hosted.model_dump() if request.self_hosted else None
    return apply_cloud_choice(
        get_agent_app(),
        mode=request.mode,
        self_hosted=self_hosted,
    )


@router.post("/finish", response_model=SetupStatus)
async def finalize_setup(request: Request) -> SetupStatus:
    """Mark the onboarding wizard complete.

    Sets ``setup_finalized=true`` in persistent state. The universal
    webapp uses this flag to gate the rest of the app surface.
    """
    setup_state.mark_finalized()
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )


@router.post("/step/{step_id}/skip", response_model=SetupStatus)
async def skip_setup_step(step_id: str, request: Request) -> SetupStatus:
    """Mark a step as deferred ("Skip for now")."""
    if step_id not in _VALID_STEP_IDS:
        raise HTTPException(status_code=404, detail=f"Unknown step id: {step_id}")
    if step_id in {"welcome", "finish"}:
        raise HTTPException(status_code=400, detail=f"Step '{step_id}' cannot be skipped")
    setup_state.mark_skipped(step_id)
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )


# ---------------------------------------------------------------------------
# Cloudflare tunnel verification + log streaming
# ---------------------------------------------------------------------------


class CloudflareVerifyResponse(BaseModel):
    reachable: bool
    status_code: int | None = None
    latency_ms: int | None = None
    target_url: str | None = None
    error: str | None = None


@router.get("/cloudflare/verify", response_model=CloudflareVerifyResponse)
async def verify_cloudflare_tunnel() -> CloudflareVerifyResponse:
    """Confirm the configured Cloudflare tunnel routes back to this agent.

    Performs an outbound HTTPS GET against the public setup URL the agent
    advertises through cloudflared. A 200 means the tunnel is up AND the
    agent is reachable through it; a non-200 or transport error means the
    operator still has work to do.
    """
    app = get_agent_app()
    cf = getattr(app.config.remote_access, "cloudflare", None)
    target = (getattr(cf, "setup_url", "") or "").strip() if cf is not None else ""
    if not target:
        return CloudflareVerifyResponse(
            reachable=False,
            error="Set the public setup URL in the Cloudflare dashboard before verifying.",
        )
    if not target.startswith(("http://", "https://")):
        return CloudflareVerifyResponse(
            reachable=False,
            target_url=target,
            error="Setup URL must start with http:// or https://.",
        )

    probe = target.rstrip("/") + "/api/v1/setup/status"
    try:
        async with httpx.AsyncClient(timeout=5.0, follow_redirects=False) as client:
            start = asyncio.get_event_loop().time()
            resp = await client.get(probe)
            latency_ms = int((asyncio.get_event_loop().time() - start) * 1000)
    except httpx.HTTPError as exc:
        return CloudflareVerifyResponse(
            reachable=False,
            target_url=target,
            error=f"Could not reach the public URL: {exc}",
        )

    return CloudflareVerifyResponse(
        reachable=resp.status_code == 200,
        status_code=resp.status_code,
        latency_ms=latency_ms,
        target_url=target,
        error=None if resp.status_code == 200 else f"Public URL returned HTTP {resp.status_code}.",
    )


# Per-unit shared journalctl tail. Spawning one subprocess per WebSocket
# subscriber wastes file descriptors and confuses the wizard if multiple
# tabs are open. We keep one tail per unit name and fan out lines to all
# connected sockets via an asyncio.Queue per subscriber.
class _JournalTail:
    def __init__(self, unit: str) -> None:
        self.unit = unit
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task[None] | None = None
        self._subscribers: set[asyncio.Queue[str]] = set()
        self._lock = asyncio.Lock()
        self._closing_task: asyncio.Task[None] | None = None

    async def subscribe(self) -> asyncio.Queue[str]:
        async with self._lock:
            if self._closing_task is not None:
                self._closing_task.cancel()
                self._closing_task = None
            queue: asyncio.Queue[str] = asyncio.Queue(maxsize=2000)
            self._subscribers.add(queue)
            if self._proc is None:
                await self._spawn()
        return queue

    async def unsubscribe(self, queue: asyncio.Queue[str]) -> None:
        async with self._lock:
            self._subscribers.discard(queue)
            if not self._subscribers and self._closing_task is None:
                self._closing_task = asyncio.create_task(self._delayed_close())

    async def _delayed_close(self) -> None:
        # Brief grace period so a tab refresh does not cycle the
        # subprocess. A subsequent subscribe() call cancels this task.
        try:
            await asyncio.sleep(10)
        except asyncio.CancelledError:
            return
        async with self._lock:
            if self._subscribers:
                self._closing_task = None
                return
            await self._terminate_proc()
            self._closing_task = None

    async def _spawn(self) -> None:
        if not shutil.which("journalctl"):
            await self._broadcast("(journalctl not available on this host)")
            return
        try:
            self._proc = await asyncio.create_subprocess_exec(
                "journalctl",
                "-u",
                self.unit,
                "-f",
                "-n",
                "120",
                "--no-pager",
                "-o",
                "short",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
        except OSError as exc:
            await self._broadcast(f"(journalctl failed to start: {exc})")
            return
        self._task = asyncio.create_task(self._reader())

    async def _reader(self) -> None:
        assert self._proc is not None and self._proc.stdout is not None
        try:
            while True:
                raw = await self._proc.stdout.readline()
                if not raw:
                    break
                # Defensive: drop lines that look like JWT-prefixed bearer
                # tokens. cloudflared itself does not log tokens, but this
                # filter shields against any future regression.
                text = raw.decode("utf-8", errors="replace").rstrip("\n")
                if "eyJ" in text and "." in text:
                    text = "(token-shaped value redacted)"
                await self._broadcast(text)
        finally:
            await self._broadcast("(journal stream ended)")

    async def _broadcast(self, line: str) -> None:
        for queue in list(self._subscribers):
            try:
                queue.put_nowait(line)
            except asyncio.QueueFull:
                # Slow consumer: drop a frame, do not stall the whole tail.
                with suppress(asyncio.QueueEmpty):
                    queue.get_nowait()
                with suppress(asyncio.QueueFull):
                    queue.put_nowait(line)

    async def _terminate_proc(self) -> None:
        if self._proc is not None:
            with suppress(ProcessLookupError):
                self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=2)
            except TimeoutError:
                with suppress(ProcessLookupError):
                    self._proc.kill()
            self._proc = None
        if self._task is not None:
            self._task.cancel()
            with suppress(asyncio.CancelledError, Exception):
                await self._task
            self._task = None


_journal_tails: dict[str, _JournalTail] = {}


def _journal_tail_for(unit: str) -> _JournalTail:
    tail = _journal_tails.get(unit)
    if tail is None:
        tail = _JournalTail(unit)
        _journal_tails[unit] = tail
    return tail


@router.websocket("/cloudflare/logs")
async def stream_cloudflare_logs(websocket: WebSocket) -> None:
    """Stream cloudflared journal lines to the wizard's log console."""
    await websocket.accept()
    app = get_agent_app()
    cf = getattr(app.config.remote_access, "cloudflare", None)
    unit = (getattr(cf, "service_name", "") or "cloudflared").strip() or "cloudflared"
    tail = _journal_tail_for(unit)
    queue = await tail.subscribe()
    try:
        while True:
            line = await queue.get()
            await websocket.send_text(line)
    except WebSocketDisconnect:
        return
    except Exception as exc:  # pragma: no cover — defensive
        log.warning("cloudflare_log_ws_error", error=str(exc))
    finally:
        await tail.unsubscribe(queue)


@router.post("/reset", response_model=SetupStatus)
async def reset_setup(request: Request) -> SetupStatus:
    """Clear setup_finalized and the skipped-step set.

    Used by the Setup page's "Re-run setup" action so the wizard
    re-engages the operator with the full step list.
    """
    setup_state.reset_state()
    return await build_setup_status(
        get_agent_app(),
        host_header=request.headers.get("host"),
    )
