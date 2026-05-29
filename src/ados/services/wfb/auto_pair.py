"""Auto-pair supervisor — RubyFPV-style first-boot bind.

Runs as a background asyncio task in the WFB service process. On every
boot, if `wfb.auto_pair_enabled` is true and the rig is not already
paired and a WFB-compatible adapter is present, the supervisor opens
the local bind window via `bind_client.forward_start_bind()` and
retries on a fixed cadence until either the bind succeeds or the
operator disables auto-pair.

After the first successful pair, `apply_keypair()` flips
`wfb.auto_pair_enabled` to false in `/etc/ados/config.yaml`. This
supervisor observes the persisted flag (via `pair_manager.status()`)
on every loop, so the next iteration sees `auto_pair_enabled == false`
and exits cleanly. A subsequent unpair leaves the flag false; the
operator must explicitly re-arm via the GCS, captive portal, REST, or
CLI to start auto-binding again.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

from ados.core.atomic import atomic_write_json
from ados.core.logging import get_logger

log = get_logger("wfb.auto_pair")

# Settle delay before the first bind attempt: lets ados-supervisor
# finish bringing services up, gives the RTL adapter time to modprobe
# and udev-rename, and lets ados-cloud get its initial heartbeat in.
START_DELAY_S = 15.0

# Backoff between retry attempts. Each attempt stops + restarts the
# normal wfb unit briefly. The bind orchestrator itself is unbounded
# in its rendezvous wait, so we only get back here when something
# actually broke (radio adapter died, tunnel never came up, etc.) —
# never on a "no peer yet" timeout.
RETRY_BACKOFF_S = 60.0

# Cap on consecutive bind FAILURES (BindError raises from the
# orchestrator) before flipping the failover sidecar to "cloud_relay".
# Real BindErrors signal a hardware-level wedge worth surfacing to the
# operator. The threshold no longer counts "no peer yet" timeouts since
# those don't exist under the unbounded rendezvous design.
MAX_LOCAL_BIND_ATTEMPTS = 10

# Sidecar file shared with the ados-api process, which exposes the
# value via GET /api/wfb/pair/failover-status. The auto_pair supervisor
# lives in ados-cloud, so a file in /run/ados is the simplest way to
# bridge the two processes without a new IPC channel.
FAILOVER_STATE_PATH = Path("/run/ados/wfb_failover.json")


class AutoPairSupervisor:
    """Background task that drives the first-boot auto-bind loop."""

    def __init__(
        self,
        role: str,
        shutdown_event: asyncio.Event | None = None,
    ) -> None:
        # role is "drone" or "gs"; comes from the agent profile fingerprint.
        # If the rig was provisioned to a non-pairing role (e.g.,
        # `relay` or `receiver` at the GS profile level) the run-loop
        # exits early without touching the radio.
        self._role = role
        self._task: asyncio.Task | None = None
        self._stop = asyncio.Event()
        # External shutdown source (the cloud service's signal handler
        # event). When it fires, mirror it into self._stop so the
        # orchestrator's cancel_event hook tears down any in-flight bind.
        self._shutdown_event = shutdown_event
        self._mirror_task: asyncio.Task | None = None
        # Tracks whether the rig is still trying to bind locally or
        # has given up and asked the operator to fall back to the
        # cloud relay. Mirror is persisted to a sidecar file so the
        # ados-api process can serve it over REST.
        self._failover_state: str = "local"

    @property
    def role(self) -> str:
        return self._role

    @property
    def failover_state(self) -> str:
        return self._failover_state

    def _persist_failover_state(self, state: str) -> None:
        """Write the failover state to /run/ados so the API can read it.

        Called whenever the supervisor flips between local-bind retries
        and cloud-relay fallback. The sidecar is read by
        GET /api/wfb/pair/failover-status which lives in a different
        process.
        """
        self._failover_state = state
        try:
            atomic_write_json(FAILOVER_STATE_PATH, {"state": state}, mode=0o644)
        except OSError as exc:
            log.warning("wfb_failover_state_persist_failed", error=str(exc))

    def start(self) -> None:
        """Spawn the supervisor task. Idempotent."""
        if self._task is not None and not self._task.done():
            return
        self._task = asyncio.create_task(self._run(), name="wfb-auto-pair")

    async def stop(self) -> None:
        """Cooperative stop of the supervisor task."""
        self._stop.set()
        if self._mirror_task is not None and not self._mirror_task.done():
            self._mirror_task.cancel()
            try:
                await self._mirror_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
            self._mirror_task = None
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
            self._task = None

    async def _mirror_external_shutdown(self) -> None:
        """Forward the service-level shutdown event into our own stop.

        The orchestrator only watches `self._stop` as its cancel source,
        so mirroring the cloud service's signal-handler event here keeps
        the orchestrator unaware of the external plumbing.
        """
        if self._shutdown_event is None:
            return
        await self._shutdown_event.wait()
        self._stop.set()

    async def _run(self) -> None:
        if self._role not in ("drone", "gs"):
            log.info("auto_pair_skipped_role", role=self._role)
            return

        # Defer all imports of state-touching modules so this file is
        # cheap to load at module-import time and tests can monkeypatch.
        from ados.core.config import load_config
        from ados.services.ground_station.pair_manager import get_pair_manager
        from ados.services.wfb.adapter import detect_wfb_adapters
        from ados.services.wfb.bind_client import BindBusyError, forward_start_bind

        # Mirror an external shutdown event into our internal stop so a
        # single cancel signal feeds the bind orchestrator. Async-safe
        # to start here (not in __init__) because we need a running loop.
        if self._shutdown_event is not None:
            self._mirror_task = asyncio.create_task(
                self._mirror_external_shutdown(),
                name="auto-pair-shutdown-mirror",
            )

        try:
            await asyncio.wait_for(self._stop.wait(), timeout=START_DELAY_S)
            return  # explicit stop during settle delay
        except TimeoutError:
            pass

        pm = get_pair_manager()

        # A fresh run resets the failover sidecar to "local" so that
        # an operator who re-armed auto-pair from the cloud-relay
        # fallback sees the rig retry local bind on the dashboard.
        self._persist_failover_state("local")

        attempt = 0
        while not self._stop.is_set():
            try:
                cfg = load_config()
            except Exception as exc:  # noqa: BLE001
                log.warning("auto_pair_config_load_failed", error=str(exc))
                await asyncio.sleep(RETRY_BACKOFF_S)
                continue

            wfb_cfg = getattr(cfg.video, "wfb", None) if hasattr(cfg, "video") else None
            if wfb_cfg is None:
                log.info("auto_pair_no_wfb_section")
                return

            if not bool(getattr(wfb_cfg, "auto_pair_enabled", False)):
                log.info("auto_pair_disarmed")
                return

            try:
                pair_status = await pm.status(self._role)
            except Exception as exc:  # noqa: BLE001
                log.warning("auto_pair_status_failed", error=str(exc))
                pair_status = {"paired": False}

            if pair_status.get("paired"):
                # Apply ran via some other path (cloud relay, manual
                # bind). Make sure the persisted flag matches reality
                # then exit.
                log.info(
                    "auto_pair_already_paired",
                    fingerprint=pair_status.get("fingerprint"),
                )
                try:
                    await pm.set_auto_pair(False, self._role)
                except Exception as exc:  # noqa: BLE001
                    log.debug("auto_pair_disarm_failed", error=str(exc))
                return

            # Need a WFB-compatible adapter before we can even try.
            try:
                adapters = detect_wfb_adapters()
            except Exception as exc:  # noqa: BLE001
                log.warning("auto_pair_adapter_detect_failed", error=str(exc))
                adapters = []

            if not any(a.is_wfb_compatible for a in adapters):
                log.info(
                    "auto_pair_no_adapter",
                    note="will retry, plug in an RTL8812EU dongle",
                    backoff_s=RETRY_BACKOFF_S,
                )
                if await self._sleep_or_stop(RETRY_BACKOFF_S):
                    return
                continue

            attempt += 1
            log.info(
                "auto_pair_attempt",
                attempt=attempt,
                role=self._role,
            )
            try:
                # Pass our internal stop event as the cancel hook so a
                # service-shutdown signal (or explicit supervisor.stop)
                # tears down any in-flight bind cleanly. The bind itself
                # is unbounded in its rendezvous wait.
                result = await forward_start_bind(
                    role=self._role,
                    source="auto",
                    peer_device_id=None,
                    cancel_event=self._stop,
                    timeout=None,
                )
            except BindBusyError:
                # Another bind path raced us (REST handler, manual CLI).
                # Defer; the busy session will succeed or fail and we
                # pick up from there.
                log.info("auto_pair_busy_retry")
                if await self._sleep_or_stop(RETRY_BACKOFF_S):
                    return
                continue
            except Exception as exc:  # noqa: BLE001 — final safety net
                log.exception("auto_pair_unexpected", error=str(exc))
                if self._maybe_failover(attempt):
                    return
                if await self._sleep_or_stop(RETRY_BACKOFF_S):
                    return
                continue

            terminal = result.get("state")
            if terminal == "paired":
                log.info(
                    "auto_pair_paired",
                    attempts=attempt,
                    fingerprint=result.get("fingerprint"),
                )
                self._persist_failover_state("local")
                # apply_keypair already flipped auto_pair_enabled to
                # false during pair persistence, so the next config
                # load above would exit; we exit here directly to
                # avoid the wasteful round trip.
                return

            if terminal == "aborted":
                # Operator or service shutdown asked us to stop. Exit
                # cleanly without bumping the failover counter.
                log.info(
                    "auto_pair_aborted",
                    session_id=result.get("session_id"),
                    reason=result.get("error"),
                )
                return

            # terminal == "failed": a real BindError surfaced (radio
            # adapter died, tunnel never came up, watchdog fired). Count
            # toward the failover threshold and try again.
            log.info(
                "auto_pair_attempt_failed",
                attempt=attempt,
                error=result.get("error"),
                backoff_s=RETRY_BACKOFF_S,
            )
            if self._maybe_failover(attempt):
                return
            if await self._sleep_or_stop(RETRY_BACKOFF_S):
                return

    def _maybe_failover(self, attempt: int) -> bool:
        """Flip the sidecar to ``cloud_relay`` after the attempt cap.

        Returns True when the supervisor should give up the local
        bind loop and fall back to the cloud relay. The cap fires on
        the Nth consecutive non-paired terminal exit so that an
        operator on a flaky bench is not stuck in a forever-retry.
        """
        if attempt >= MAX_LOCAL_BIND_ATTEMPTS:
            self._persist_failover_state("cloud_relay")
            log.warning("wfb_failover_to_cloud_relay", attempts=attempt)
            return True
        return False

    async def _sleep_or_stop(self, seconds: float) -> bool:
        """Sleep `seconds` or return early when stop is signalled.

        Returns True if stopped, False if the sleep elapsed normally.
        """
        try:
            await asyncio.wait_for(self._stop.wait(), timeout=seconds)
            return True
        except TimeoutError:
            return False


# ---------------------------------------------------------------------
# Module-level singleton helpers
# ---------------------------------------------------------------------
_instance: AutoPairSupervisor | None = None


def get_auto_pair_supervisor(
    role: str,
    shutdown_event: asyncio.Event | None = None,
) -> AutoPairSupervisor:
    """Return the process-wide AutoPairSupervisor singleton.

    The role and shutdown_event parameters are used only on first
    construction; subsequent calls return the existing instance
    regardless of the arguments passed.
    """
    global _instance
    if _instance is None:
        _instance = AutoPairSupervisor(role=role, shutdown_event=shutdown_event)
    return _instance


def _reset_for_tests() -> None:
    """Drop the cached singleton. Test-only helper."""
    global _instance
    _instance = None
