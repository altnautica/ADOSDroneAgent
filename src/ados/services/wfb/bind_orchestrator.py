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

# End-to-end timeout (15s tunnel + ~15s protocol + slack).
DEFAULT_TIMEOUT_S = 60.0
TUNNEL_WAIT_TIMEOUT_S = 30.0
TUNNEL_POLL_INTERVAL_S = 1.0


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

    def to_dict(self) -> dict:
        return {
            "session_id": self.session_id,
            "role": self.role,
            "state": self.state.value,
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
        timeout_s: float = DEFAULT_TIMEOUT_S,
    ) -> dict:
        """Open a bind window and run the upstream protocol to completion.

        Returns the final session dict regardless of success/failure.
        Concurrent calls fail-fast with a `409`-style ConflictError.
        """
        if self._lock.locked():
            raise BindBusyError("a bind session is already in progress")

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
            try:
                await asyncio.wait_for(
                    self._run_session(session, peer_device_id),
                    timeout=timeout_s,
                )
            except asyncio.TimeoutError:
                session.error = f"timeout after {timeout_s}s"
                session.state = BindState.FAILED
                log.warning(
                    "bind_session_timeout",
                    session_id=session.session_id,
                    timeout_s=timeout_s,
                )
                await self._cleanup(session)
            except BindError as exc:
                session.error = str(exc)
                session.state = BindState.FAILED
                log.warning(
                    "bind_session_failed",
                    session_id=session.session_id,
                    error=str(exc),
                )
                await self._cleanup(session)
            except Exception as exc:  # noqa: BLE001 — final safety net
                session.error = f"unexpected: {exc}"
                session.state = BindState.FAILED
                log.exception("bind_session_crashed", session_id=session.session_id)
                await self._cleanup(session)
            finally:
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

        session.state = BindState.OPENING_TUNNEL

        # Stage 3: Start the bind profile. Brings up the L3 tunnel.
        ok, err = _systemctl("start", bind_unit)
        if not ok:
            raise BindError(f"failed to start {bind_unit}: {err}")

        try:
            tunnel_up = await _wait_for_iface(bind_iface, TUNNEL_WAIT_TIMEOUT_S)
            if not tunnel_up:
                raise BindError(
                    f"bind tunnel interface {bind_iface} did not come up "
                    f"within {TUNNEL_WAIT_TIMEOUT_S}s"
                )

            session.state = BindState.WAITING_PEER

            # Stage 4: Run the upstream wire protocol.
            if session.role == "drone":
                await self._run_drone_server(session)
            else:
                await self._run_gs_client(session)

            session.state = BindState.APPLYING_KEYS

            # Stage 5: Copy the resulting upstream key file into the
            # agent's canonical slot via PairManager. PairManager also
            # persists pair state and restarts the normal wfb unit, so
            # we don't need to do it ourselves below.
            from ados.services.ground_station.pair_manager import (
                get_pair_manager,
            )
            from ados.services.wfb.key_mgr import read_public_fingerprint

            pm = get_pair_manager()
            if session.role == "drone":
                if not UPSTREAM_DRONE_KEY.is_file():
                    raise BindError(
                        f"bind protocol completed but {UPSTREAM_DRONE_KEY} not "
                        "present. Upstream server may have failed silently."
                    )
                blob = UPSTREAM_DRONE_KEY.read_bytes()
            else:
                if not UPSTREAM_GS_KEY.is_file():
                    raise BindError(
                        f"bind protocol completed but {UPSTREAM_GS_KEY} not "
                        "present. wfb_keygen output may have been overwritten."
                    )
                blob = UPSTREAM_GS_KEY.read_bytes()

            session.state = BindState.RESTARTING_SERVICES
            await pm.apply_keypair(blob, session.role, peer_device_id)

            try:
                target = pm.tx_key_path if session.role == "drone" else pm.rx_key_path
                session.fingerprint = read_public_fingerprint(target)
            except (OSError, ValueError) as exc:
                log.debug(
                    "fingerprint_post_apply_failed",
                    session_id=session.session_id,
                    error=str(exc),
                )

            session.state = BindState.PAIRED
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

        session.state = BindState.TRANSFERRING_KEYS
        rc, stdout, stderr = await _run_socat_with_kill_on_cancel(
            cmd,
            log_event="bind_server_socat",
            session_id=session.session_id,
        )
        if rc != 0:
            raise BindError(
                f"socat server exited rc={rc}: "
                f"{stderr.decode(errors='replace').strip()[:240]}"
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

        # Retry budget must outlast the drone server's hold time so the
        # two ends overlap reliably under auto-pair, where the two
        # sessions start on independent cadences. With DEFAULT_TIMEOUT_S
        # at 60s and tunnel bring-up taking 1-3s, retry=55 keeps the
        # client probing for ~55s before socat itself gives up.
        cmd = [
            "socat",
            "-d",
            f"TCP4:{DRONE_BIND_PEER_IP}:{BIND_TCP_PORT},crlf,retry=55,interval=1",
            f"EXEC:{WFB_BIND_CLIENT_SH}",
        ]
        log.info(
            "bind_client_socat",
            session_id=session.session_id,
            cmd=" ".join(cmd),
        )

        session.state = BindState.TRANSFERRING_KEYS
        rc, stdout, stderr = await _run_socat_with_kill_on_cancel(
            cmd,
            log_event="bind_client_socat",
            session_id=session.session_id,
        )
        if rc != 0:
            raise BindError(
                f"socat client exited rc={rc}: "
                f"{stderr.decode(errors='replace').strip()[:240]}"
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
    """Raised when a bind session fails for a recoverable reason."""


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
