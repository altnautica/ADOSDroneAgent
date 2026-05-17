"""Local-radio WFB bind orchestrator.

Wraps upstream wfb-ng's local bind protocol so a paired drone + ground
station come up encrypted with zero operator-visible key handling. The
upstream protocol (`scripts/bind/init_drone.sh`, `init_gs.sh`,
`wfb_bind_server.sh`, `wfb_bind_client.sh`) opens an L3 IP tunnel over
the radio using a separate `*_bind` wfb-ng profile and a hardcoded
default `bind.key`, then streams a tar.gz of the matching `drone.key`
and config over a single TCP socket. Once the GS has shipped its
generated `drone.key` to the drone (or vice versa), both rigs flip
back to the encrypted `drone` / `gs` profile and the link comes up.

This module:
- Owns the systemd transitions (stop normal wfb unit, start bind
  profile, run socat over the bind tunnel, stop bind profile, start
  normal wfb unit).
- Generates the keypair via `wfb_keygen` on the GS side (or accepts
  whatever the GS sends on the drone side).
- After the upstream shell scripts complete, copies the resulting
  `/etc/drone.key` (drone) or `/etc/gs.key` (GS) into the agent's
  canonical `/etc/ados/wfb/{tx,rx}.key` slot via PairManager.
- Surfaces a state machine the REST + LCD + GCS surfaces can poll.

Single-instance per agent process. Concurrent bind requests get a
409 from the orchestrator. A 60-second hard timeout aborts a stuck
session and rolls back to the normal wfb profile.
"""

from __future__ import annotations

import asyncio
import shutil
import subprocess
import time
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from typing import Literal

from ados.core.logging import get_logger

log = get_logger("wfb.bind_orchestrator")

Role = Literal["drone", "gs"]

# Upstream wfb-ng paths. install.sh ensures /etc/bind.yaml + /etc/bind.key
# exist with the upstream defaults so the bind profile can come up.
UPSTREAM_BIND_KEY = Path("/etc/bind.key")
UPSTREAM_BIND_YAML = Path("/etc/bind.yaml")
UPSTREAM_DRONE_KEY = Path("/etc/drone.key")
UPSTREAM_GS_KEY = Path("/etc/gs.key")
UPSTREAM_WIFIBROADCAST_CFG = Path("/etc/wifibroadcast.cfg")

# Upstream shell scripts that own the wire protocol.
WFB_BIND_SERVER_SH = "/usr/bin/wfb_bind_server.sh"
WFB_BIND_CLIENT_SH = "/usr/bin/wfb_bind_client.sh"

# wfb-ng systemd template unit names.
DRONE_BIND_UNIT = "wifibroadcast@drone_bind.service"
GS_BIND_UNIT = "wifibroadcast@gs_bind.service"

# Agent-managed normal-operation wfb units.
ADOS_WFB_DRONE_UNIT = "ados-wfb.service"
ADOS_WFB_GS_UNIT = "ados-wfb-rx.service"

# Tunnel interfaces created by the bind profile (10.5.99.x L3 over WFB).
DRONE_BIND_IFACE = "drone-bind"
GS_BIND_IFACE = "gs-bind"
DRONE_BIND_PEER_IP = "10.5.99.2"
BIND_TCP_PORT = 5555

# Tunnel bring-up is the only real wall-clock failure mode. socat itself
# blocks on accept() (drone server) or retries connect() (gs client) for
# as long as we let it; the rendezvous between the two halves is unbounded
# by design so a slow-to-boot peer never causes a "missed window" failure.
TUNNEL_WAIT_TIMEOUT_S = 30.0
TUNNEL_POLL_INTERVAL_S = 1.0

# Belt-and-suspenders watchdog for a wedged session that neither makes
# progress nor surfaces as an OS-level error (rc!=0). Sized big enough
# that an operator on a flaky bench rig never sees this fire under
# normal "still looking for peer" conditions.
WAITING_PEER_WATCHDOG_S = 1800.0  # 30 minutes

# Per-phase sub-timeouts. The global watchdog above catches wedged-but-
# alive sessions; these catch a specific phase that hangs while every
# other liveness signal still looks fine. Sized as wall-clock budgets
# that real bench sessions never approach.
#
# Key transfer (TRANSFERRING_KEYS + APPLYING_KEYS combined): the upstream
# wfb_bind_{server,client}.sh wrappers stream a ~4 KB tar.gz over the
# L3 tunnel. Even on a lossy link, anything past 5 minutes means the
# socat handshake has stalled and won't recover on its own.
KEY_TRANSFER_TIMEOUT_S = 300.0

# Service restart (RESTARTING_SERVICES): PairManager.apply_keypair()
# writes the new key file and `systemctl restart`s the normal wfb
# unit. The restart itself is bounded by systemd's own timeouts, but
# a stuck dependency or wedged drop-in can outlast them. 60s is an
# order of magnitude above the steady-state restart cost.
RESTART_TIMEOUT_S = 60.0


class BindState(StrEnum):
    IDLE = "idle"
    OPENING_TUNNEL = "opening_tunnel"
    WAITING_PEER = "waiting_peer"
    TRANSFERRING_KEYS = "transferring_keys"
    APPLYING_KEYS = "applying_keys"
    RESTARTING_SERVICES = "restarting_services"
    PAIRED = "paired"
    FAILED = "failed"
    ABORTED = "aborted"


@dataclass
class BindSession:
    session_id: str
    role: Role
    state: BindState = BindState.IDLE
    started_at: str = field(default_factory=lambda: datetime.now(UTC).isoformat(timespec="seconds"))
    finished_at: str | None = None
    error: str | None = None
    fingerprint: str | None = None
    peer_device_id: str | None = None
    source: str = "operator"  # "operator" | "auto"
    # Monotonic timestamp marking the most recent state transition. Used
    # to compute `phase_age_s` for the REST surface so the LCD/GCS can
    # render "stuck in TRANSFERRING_KEYS for 47s" without dragging system
    # wall-clock into the math.
    phase_entered_at: float | None = None

    @property
    def phase(self) -> str:
        """String alias of the current state. Pairs with `phase_entered_at`
        so consumers reading from JSON get a phase tag + age clock without
        having to know the BindState enum."""
        return self.state.value

    def transition(self, new_state: BindState) -> None:
        """Move to a new state and stamp the phase clock. Centralises the
        pairing of `state` mutations with `phase_entered_at` so a future
        contributor can't update one without the other."""
        self.state = new_state
        self.phase_entered_at = time.monotonic()

    def to_dict(self) -> dict:
        phase_age_s: float | None = None
        if self.phase_entered_at is not None:
            phase_age_s = max(0.0, time.monotonic() - self.phase_entered_at)
        return {
            "session_id": self.session_id,
            "role": self.role,
            "state": self.state.value,
            "phase": self.phase,
            "phase_entered_at": self.phase_entered_at,
            "phase_age_s": phase_age_s,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "error": self.error,
            "fingerprint": self.fingerprint,
            "peer_device_id": self.peer_device_id,
            "source": self.source,
        }


def _systemctl(action: str, unit: str, timeout_s: float = 10.0) -> tuple[bool, str]:
    """Invoke systemctl, return (ok, stderr_text)."""
    try:
        result = subprocess.run(
            ["systemctl", action, unit],
            check=False,
            capture_output=True,
            timeout=timeout_s,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return False, str(exc)
    if result.returncode != 0:
        return False, result.stderr.decode(errors="replace").strip()
    return True, ""


def _kill_stale_bind_socats() -> None:
    """Kill leftover socat processes from a previously aborted bind session.

    Why: Python's asyncio cancels the wrapper task on outer wait_for timeout,
    but `asyncio.subprocess.Process` does not propagate the cancel to the
    underlying OS process. A bind session that hits the 60s outer timeout
    leaves the socat listener (drone) or socat client (gs) running with
    port 5555 on 10.5.99.2 still bound. Every subsequent attempt then
    crashes with `bind() Address already in use`. This helper sweeps
    those stragglers before opening a new session and during cleanup.
    """
    patterns = (
        f"socat.*TCP4-LISTEN:{BIND_TCP_PORT},bind={DRONE_BIND_PEER_IP}",
        f"socat.*TCP4:{DRONE_BIND_PEER_IP}:{BIND_TCP_PORT}",
    )
    for pattern in patterns:
        try:
            subprocess.run(
                ["pkill", "-9", "-f", pattern],
                check=False,
                timeout=5.0,
                capture_output=True,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            log.debug("stale_bind_socat_kill_failed", pattern=pattern, error=str(exc))


async def _run_socat_with_kill_on_cancel(
    cmd: list[str],
    *,
    log_event: str,
    session_id: str,
) -> tuple[int, bytes, bytes]:
    """Run a socat subprocess and guarantee its OS process dies on cancel.

    `asyncio.create_subprocess_exec` returns a Process whose lifetime is
    tied to its handle, not to the caller's task. If the caller is
    cancelled mid-`communicate()` (e.g., the orchestrator's outer
    `asyncio.wait_for` fires), the OS process keeps running and holds
    its sockets. Wrapping the await in try/finally with proc.kill() in
    the cancel path is what closes the gap.
    """
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        stdout, stderr = await proc.communicate()
        return proc.returncode or 0, stdout, stderr
    finally:
        if proc.returncode is None:
            log.warning(
                f"{log_event}_force_killed",
                session_id=session_id,
                pid=proc.pid,
            )
            try:
                proc.kill()
            except ProcessLookupError:
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except (asyncio.TimeoutError, ProcessLookupError):
                pass


async def _wait_for_iface(iface: str, timeout_s: float = TUNNEL_WAIT_TIMEOUT_S) -> bool:
    """Poll `ip -4 addr show` until the L3 bind tunnel interface appears."""
    deadline = asyncio.get_event_loop().time() + timeout_s
    while asyncio.get_event_loop().time() < deadline:
        result = await asyncio.create_subprocess_exec(
            "ip", "-4", "addr", "show", "dev", iface,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        await result.wait()
        if result.returncode == 0:
            return True
        await asyncio.sleep(TUNNEL_POLL_INTERVAL_S)
    return False


class BindOrchestrator:
    """Single-instance state machine that drives a local-radio bind window."""

    def __init__(self) -> None:
        self._lock = asyncio.Lock()
        self._session: BindSession | None = None

    @property
    def session(self) -> BindSession | None:
        return self._session

    async def status(self) -> dict | None:
        """Snapshot of the current session, or None if idle."""
        if self._session is None:
            return None
        return self._session.to_dict()

    async def start_local_bind(
        self,
        role: Role,
        peer_device_id: str | None = None,
        source: str = "operator",
        cancel_event: asyncio.Event | None = None,
    ) -> dict:
        """Open a bind window and run the upstream protocol to completion.

        Pairing is a rendezvous: the call blocks until either a peer is
        found and the bind handshake succeeds, the caller fires
        `cancel_event` (operator abort, service shutdown), the watchdog
        fires (a wedge no one is detecting at the OS level), or the
        protocol itself raises `BindError`. There is intentionally NO
        time bound on "still looking for peer" — the previous bounded-
        window design produced a phase-alignment race between the drone
        and gs halves that broke auto-pair on lopsided availability.
        Concurrent calls fail-fast with a `409`-style ConflictError.
        """
        if self._lock.locked():
            raise BindBusyError("a bind session is already in progress")

        # A never-firing default lets callers omit the parameter when
        # they don't have an external cancel source. The watchdog still
        # bounds a wedged session.
        cancel_event = cancel_event if cancel_event is not None else asyncio.Event()

        async with self._lock:
            # Sweep stragglers from any prior aborted session before we
            # touch the radio. Cheap and idempotent when nothing is stale.
            _kill_stale_bind_socats()

            session = BindSession(
                session_id=str(uuid.uuid4()),
                role=role,
                source=source,
                peer_device_id=peer_device_id,
            )
            self._session = session
            log.info(
                "bind_session_started",
                session_id=session.session_id,
                role=role,
                source=source,
            )
            session_task = asyncio.create_task(
                self._run_session(session, peer_device_id),
                name=f"bind-session-{session.session_id[:8]}",
            )
            cancel_task = asyncio.create_task(
                cancel_event.wait(),
                name=f"bind-cancel-{session.session_id[:8]}",
            )
            watchdog_task = asyncio.create_task(
                asyncio.sleep(WAITING_PEER_WATCHDOG_S),
                name=f"bind-watchdog-{session.session_id[:8]}",
            )
            try:
                done, _pending = await asyncio.wait(
                    {session_task, cancel_task, watchdog_task},
                    return_when=asyncio.FIRST_COMPLETED,
                )
                if cancel_task in done and session_task not in done:
                    # Operator-driven abort (CLI, GCS, webapp toggle, or
                    # service shutdown). Kill any in-flight socat via
                    # session_task cancellation; its finally hooks then
                    # drop the OS process.
                    session.transition(BindState.ABORTED)
                    session.error = "cancelled by caller"
                    log.info(
                        "bind_session_aborted",
                        session_id=session.session_id,
                    )
                    await self._cleanup(session)
                elif watchdog_task in done and session_task not in done:
                    session.transition(BindState.FAILED)
                    session.error = (
                        f"watchdog fired after {WAITING_PEER_WATCHDOG_S}s "
                        "with no progress"
                    )
                    log.warning(
                        "bind_session_watchdog_fired",
                        session_id=session.session_id,
                        watchdog_s=WAITING_PEER_WATCHDOG_S,
                    )
                    await self._cleanup(session)
                else:
                    # session_task completed (exception or success).
                    exc = session_task.exception()
                    if isinstance(exc, BindError):
                        session.error = str(exc)
                        # Preserve the phase the error surfaced in (set by
                        # the raiser via BindError(phase=...)) when present;
                        # otherwise fall through to the last state the
                        # session reached. This keeps the LCD/GCS able to
                        # render "key transfer timed out" with the right
                        # phase badge.
                        failure_phase = getattr(exc, "phase", None)
                        session.transition(BindState.FAILED)
                        log.warning(
                            "bind_session_failed",
                            session_id=session.session_id,
                            error=str(exc),
                            phase=failure_phase,
                        )
                        await self._cleanup(session)
                    elif exc is not None:
                        session.error = f"unexpected: {exc}"
                        session.transition(BindState.FAILED)
                        log.exception(
                            "bind_session_crashed",
                            session_id=session.session_id,
                        )
                        await self._cleanup(session)
                    # else: session.state already set inside _run_session
                    # (PAIRED on the happy path).
            finally:
                # Always cancel the helpers that didn't complete so they
                # don't leak past this scope. session_task's finally
                # already kills any socat subprocess on cancel.
                for task in (session_task, cancel_task, watchdog_task):
                    if not task.done():
                        task.cancel()
                        try:
                            await task
                        except (asyncio.CancelledError, Exception):  # noqa: BLE001
                            pass
                session.finished_at = datetime.now(UTC).isoformat(timespec="seconds")

        return session.to_dict()

    async def _run_session(
        self,
        session: BindSession,
        peer_device_id: str | None,
    ) -> None:
        """End-to-end orchestration. Stages mirror BindState transitions."""
        # Pre-flight: every external dep the bind protocol needs must
        # be present BEFORE we touch the radio. A missing socat / shell
        # script / bind artifact would otherwise surface ~6 retries
        # later as a generic 'unexpected: FileNotFoundError' from deep
        # inside an asyncio.create_subprocess_exec call. Failing fast
        # with a structured BindError is faster to debug.
        for path in (UPSTREAM_BIND_KEY, UPSTREAM_BIND_YAML):
            if not path.is_file():
                raise BindError(
                    f"upstream wfb-ng artifact missing: {path}. Reinstall via "
                    "install.sh to provision /etc/bind.key and /etc/bind.yaml."
                )
        for script in (WFB_BIND_SERVER_SH, WFB_BIND_CLIENT_SH):
            if not Path(script).is_file():
                raise BindError(
                    f"upstream wfb-ng helper missing: {script}. wfb-ng "
                    "package must be installed via install.sh."
                )
        if shutil.which("socat") is None:
            raise BindError(
                "socat binary not found on PATH. Install via "
                "`apt install socat` or rerun install.sh which now "
                "includes socat in its deps."
            )

        normal_unit = (
            ADOS_WFB_DRONE_UNIT if session.role == "drone" else ADOS_WFB_GS_UNIT
        )
        bind_unit = (
            DRONE_BIND_UNIT if session.role == "drone" else GS_BIND_UNIT
        )
        bind_iface = (
            DRONE_BIND_IFACE if session.role == "drone" else GS_BIND_IFACE
        )

        # Stage 1: GS-only — generate the fresh keypair BEFORE touching
        # the wfb units. Cheap operation; if it fails (e.g., wfb_keygen
        # absent), the normal wfb unit stays running and the only
        # disruption is a logged failure. Done here, BEFORE the
        # _systemctl("stop"), so a wfb_keygen failure doesn't leave the
        # rig with no wfb service running.
        if session.role == "gs":
            await self._generate_keypair_or_raise()

        # Stage 2: Stop the normal wfb unit so it releases the radio
        # adapter for the bind profile. After this point any failure
        # path MUST restart the normal unit; the try/finally in
        # start_local_bind ensures _cleanup runs.
        _systemctl("stop", normal_unit)

        session.transition(BindState.OPENING_TUNNEL)

        # Stage 3: Start the bind profile. Brings up the L3 tunnel.
        ok, err = _systemctl("start", bind_unit)
        if not ok:
            raise BindError(
                f"failed to start {bind_unit}: {err}",
                phase=BindState.OPENING_TUNNEL.value,
            )

        try:
            tunnel_up = await _wait_for_iface(bind_iface, TUNNEL_WAIT_TIMEOUT_S)
            if not tunnel_up:
                raise BindError(
                    f"bind tunnel interface {bind_iface} did not come up "
                    f"within {TUNNEL_WAIT_TIMEOUT_S}s",
                    phase=BindState.OPENING_TUNNEL.value,
                )

            session.transition(BindState.WAITING_PEER)

            # Stage 4: Run the upstream wire protocol. The drone/gs
            # helpers move the state to TRANSFERRING_KEYS the moment
            # they hand off to socat. We wrap both the socat exchange
            # AND the immediately-following APPLYING_KEYS work in a
            # single `asyncio.wait_for` budget so a stuck socat or a
            # stuck post-handshake key read never wedges the rig past
            # KEY_TRANSFER_TIMEOUT_S.
            try:
                async with asyncio.timeout(KEY_TRANSFER_TIMEOUT_S):
                    if session.role == "drone":
                        await self._run_drone_server(session)
                    else:
                        await self._run_gs_client(session)

                    session.transition(BindState.APPLYING_KEYS)

                    # Stage 5: Copy the resulting upstream key file into the
                    # agent's canonical slot via PairManager. PairManager also
                    # persists pair state and restarts the normal wfb unit, so
                    # we don't need to do it ourselves below.
                    from ados.services.ground_station.pair_manager import (
                        get_pair_manager,
                    )
                    from ados.services.wfb.key_mgr import (
                        read_public_fingerprint,
                    )

                    pm = get_pair_manager()
                    if session.role == "drone":
                        if not UPSTREAM_DRONE_KEY.is_file():
                            raise BindError(
                                f"bind protocol completed but "
                                f"{UPSTREAM_DRONE_KEY} not present. "
                                "Upstream server may have failed silently.",
                                phase=BindState.APPLYING_KEYS.value,
                            )
                        blob = UPSTREAM_DRONE_KEY.read_bytes()
                    else:
                        if not UPSTREAM_GS_KEY.is_file():
                            raise BindError(
                                f"bind protocol completed but "
                                f"{UPSTREAM_GS_KEY} not present. "
                                "wfb_keygen output may have been "
                                "overwritten.",
                                phase=BindState.APPLYING_KEYS.value,
                            )
                        blob = UPSTREAM_GS_KEY.read_bytes()
            except TimeoutError as exc:
                # The combined transfer + apply budget tripped. Tag the
                # error with whichever phase we were in when the budget
                # ran out so the GCS renders the right badge.
                raise BindError(
                    f"key transfer timed out after "
                    f"{KEY_TRANSFER_TIMEOUT_S}s in {session.state.value}",
                    phase=session.state.value,
                ) from exc

            # Stage 6: Restart the agent-managed wfb unit so it picks
            # up the new key. Wrapped in its own wait_for so a stuck
            # systemd restart can't wedge the rig past RESTART_TIMEOUT_S.
            session.transition(BindState.RESTARTING_SERVICES)
            try:
                async with asyncio.timeout(RESTART_TIMEOUT_S):
                    await pm.apply_keypair(blob, session.role, peer_device_id)
            except TimeoutError as exc:
                raise BindError(
                    f"service restart timed out after {RESTART_TIMEOUT_S}s",
                    phase=BindState.RESTARTING_SERVICES.value,
                ) from exc

            try:
                target = pm.tx_key_path if session.role == "drone" else pm.rx_key_path
                session.fingerprint = read_public_fingerprint(target)
            except (OSError, ValueError) as exc:
                log.debug(
                    "fingerprint_post_apply_failed",
                    session_id=session.session_id,
                    error=str(exc),
                )

            session.transition(BindState.PAIRED)
            log.info(
                "bind_session_paired",
                session_id=session.session_id,
                role=session.role,
                fingerprint=session.fingerprint,
            )
        finally:
            # Always tear down the bind profile so we don't leave the
            # rig stuck on the unencrypted bind tunnel after a failure.
            _systemctl("stop", bind_unit)

    async def _generate_keypair_or_raise(self) -> None:
        """Run `wfb_keygen` in /etc so /etc/gs.key + /etc/drone.key land in
        the locations wfb_bind_client.sh expects them.
        """
        if shutil.which("wfb_keygen") is None:
            raise BindError(
                "wfb_keygen binary not found on PATH. install.sh provisions "
                "wfb-ng with the keygen tool; reinstall the agent."
            )

        # Atomic-ish: remove any stale leftovers first so we don't ship
        # mismatched halves. wfb_keygen writes both files together.
        for path in (UPSTREAM_GS_KEY, UPSTREAM_DRONE_KEY):
            try:
                if path.is_file():
                    path.unlink()
            except OSError as exc:
                log.debug("stale_key_unlink_failed", path=str(path), error=str(exc))

        proc = await asyncio.create_subprocess_exec(
            "wfb_keygen",
            cwd="/etc",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            raise BindError(
                f"wfb_keygen failed (rc={proc.returncode}): "
                f"{stderr.decode(errors='replace').strip()}"
            )
        if not UPSTREAM_GS_KEY.is_file() or not UPSTREAM_DRONE_KEY.is_file():
            raise BindError(
                f"wfb_keygen exited 0 but did not produce {UPSTREAM_GS_KEY} + "
                f"{UPSTREAM_DRONE_KEY}"
            )
        log.info("wfb_keygen_complete", stdout=stdout.decode(errors="replace").strip())

    async def _run_drone_server(self, session: BindSession) -> None:
        """Drone side. Listen on TCP/5555 over the bind tunnel."""
        if not Path(WFB_BIND_SERVER_SH).is_file():
            raise BindError(
                f"upstream {WFB_BIND_SERVER_SH} missing. wfb-ng package "
                "must be installed."
            )

        cmd = [
            "socat",
            "-d",
            f"TCP4-LISTEN:{BIND_TCP_PORT},bind={DRONE_BIND_PEER_IP},reuseaddr,crlf",
            f"EXEC:{WFB_BIND_SERVER_SH}",
        ]
        log.info(
            "bind_server_socat",
            session_id=session.session_id,
            cmd=" ".join(cmd),
        )

        session.transition(BindState.TRANSFERRING_KEYS)
        rc, stdout, stderr = await _run_socat_with_kill_on_cancel(
            cmd,
            log_event="bind_server_socat",
            session_id=session.session_id,
        )
        if rc != 0:
            raise BindError(
                f"socat server exited rc={rc}: "
                f"{stderr.decode(errors='replace').strip()[:240]}",
                phase=BindState.TRANSFERRING_KEYS.value,
            )
        log.info(
            "bind_server_complete",
            session_id=session.session_id,
            stdout=stdout.decode(errors="replace").strip()[:240],
        )

    async def _run_gs_client(self, session: BindSession) -> None:
        """GS side. Connect to the drone's TCP listener over the tunnel."""
        if not Path(WFB_BIND_CLIENT_SH).is_file():
            raise BindError(
                f"upstream {WFB_BIND_CLIENT_SH} missing. wfb-ng package "
                "must be installed."
            )

        # Unbounded rendezvous: socat retries connect() at 1s interval
        # for as long as the orchestrator keeps it alive. The orchestrator
        # itself ends the wait via cancel_event (operator abort or service
        # shutdown), watchdog (wedged session paranoia), or the success
        # path. retry=86400 (24h) is functionally infinite — a real
        # bench session pairs in seconds when the peer is up.
        cmd = [
            "socat",
            "-d",
            f"TCP4:{DRONE_BIND_PEER_IP}:{BIND_TCP_PORT},crlf,retry=86400,interval=1",
            f"EXEC:{WFB_BIND_CLIENT_SH}",
        ]
        log.info(
            "bind_client_socat",
            session_id=session.session_id,
            cmd=" ".join(cmd),
        )

        session.transition(BindState.TRANSFERRING_KEYS)
        rc, stdout, stderr = await _run_socat_with_kill_on_cancel(
            cmd,
            log_event="bind_client_socat",
            session_id=session.session_id,
        )
        if rc != 0:
            raise BindError(
                f"socat client exited rc={rc}: "
                f"{stderr.decode(errors='replace').strip()[:240]}",
                phase=BindState.TRANSFERRING_KEYS.value,
            )
        log.info(
            "bind_client_complete",
            session_id=session.session_id,
            stdout=stdout.decode(errors="replace").strip()[:240],
        )

    async def _cleanup(self, session: BindSession) -> None:
        """Restart the normal wfb unit after a failed bind session.

        On success path PairManager.apply_keypair() already restarted
        the unit; on failure path we restart it here so the rig is
        never left with both bind and normal profiles stopped.
        """
        normal_unit = (
            ADOS_WFB_DRONE_UNIT if session.role == "drone" else ADOS_WFB_GS_UNIT
        )
        bind_unit = (
            DRONE_BIND_UNIT if session.role == "drone" else GS_BIND_UNIT
        )
        # Belt-and-suspenders: any socat that survived the cancel path
        # would leak port 5555 and break the next attempt. The kill is
        # cheap and idempotent.
        _kill_stale_bind_socats()
        _systemctl("stop", bind_unit)
        _systemctl("start", normal_unit)


class BindError(RuntimeError):
    """Raised when a bind session fails for a recoverable reason.

    The optional `phase` attribute names the BindState the orchestrator
    was in when the failure surfaced. REST/LCD consumers use it to
    render "key transfer timed out" with the right phase badge instead
    of a generic "bind failed". Default `None` keeps the old
    `raise BindError("msg")` call sites working.
    """

    def __init__(self, message: str, phase: str | None = None) -> None:
        super().__init__(message)
        self.phase = phase


class BindBusyError(BindError):
    """Raised when a second bind session is requested while one is running."""


# ---------------------------------------------------------------------
# Module-level singleton
# ---------------------------------------------------------------------
_instance: "BindOrchestrator | None" = None


def get_bind_orchestrator() -> "BindOrchestrator":
    """Process-wide BindOrchestrator singleton."""
    global _instance
    if _instance is None:
        _instance = BindOrchestrator()
    return _instance


def _reset_for_tests() -> None:
    """Drop the cached singleton. Test-only helper."""
    global _instance
    _instance = None


# Terminal states. A session in any of these is finished; the radio is
# back under normal-unit control (or never left it) and unrelated
# supervisors are free to act on the wfb adapter again.
_TERMINAL_BIND_STATES: frozenset[BindState] = frozenset(
    {
        BindState.IDLE,
        BindState.PAIRED,
        BindState.FAILED,
        BindState.ABORTED,
    }
)


def is_bind_active() -> bool:
    """Return True if a bind session is currently in flight.

    Terminal states (IDLE, PAIRED, FAILED, ABORTED) return False.
    Non-terminal states (OPENING_TUNNEL, WAITING_PEER, TRANSFERRING_KEYS,
    APPLYING_KEYS, RESTARTING_SERVICES) return True.

    Used by the agent supervisor to skip auto-restart of ados-wfb /
    ados-wfb-rx during bind so wfb-ng owns the radio adapter
    exclusively for the handshake. Also consulted by the hop
    supervisor to skip channel changes while a bind is in flight.
    """
    if _instance is None:
        return False
    session = _instance.session
    if session is None:
        return False
    return session.state not in _TERMINAL_BIND_STATES
