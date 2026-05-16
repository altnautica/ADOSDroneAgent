"""Cloud posture + Cloudflare tunnel routes (configure, verify, log stream, reboot)."""

from __future__ import annotations

import asyncio
import shutil
from contextlib import suppress

import httpx
from fastapi import APIRouter, WebSocket, WebSocketDisconnect

from ados.api.deps import get_agent_app
from ados.setup.models import SetupActionResult
from ados.setup.service import apply_cloud_choice, install_cloudflare_token

from ._common import log
from ._models import (
    CloudChoiceRequest,
    CloudflareTokenRequest,
    CloudflareVerifyResponse,
)

router = APIRouter()


@router.post("/remote-access/cloudflare", response_model=SetupActionResult)
async def configure_cloudflare_tunnel(request: CloudflareTokenRequest) -> SetupActionResult:
    """Install a remotely managed Cloudflare Tunnel token or install command."""
    return install_cloudflare_token(get_agent_app(), request.token_or_script)


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
    """Stream cloudflared journal lines to the wizard's log console.

    The HTTP middleware does not process WebSocket handshakes, so the
    paired-key check runs inline here. Native clients pass
    ``X-ADOS-Key`` on the handshake; browsers mint a one-shot ticket
    via ``POST /api/_ws/ticket`` with ``scope=setup.cloudflare_logs``
    and present it through the ``ados-ws-ticket`` subprotocol.
    """
    from ados.api.middleware.ws_auth import authenticate_websocket as _ws_auth

    accept_subprotocol = await _ws_auth(
        websocket, scope="setup.cloudflare_logs"
    )
    if accept_subprotocol is None:
        return
    if accept_subprotocol:
        await websocket.accept(subprotocol=accept_subprotocol)
    else:
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
