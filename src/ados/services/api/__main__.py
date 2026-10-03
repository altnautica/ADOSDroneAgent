"""Standalone REST API service.

Runs the residual FastAPI app with uvicorn on the internal Unix socket the
native control front proxies to, connecting to state IPC for live telemetry.

Run: python -m ados.services.api
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog
import uvicorn

from ados.core.config import load_config
from ados.core.ipc import StateIPCClient
from ados.core.logging import configure_logging
from ados.core.paths import ADOS_RUN_DIR

_PROFILE_SEED_DELAY_S = 5.0
_PROFILE_SEED_RETRY_GAP_S = 30.0
_PROFILE_SEED_MAX_RETRIES = 4

# Plain-text sidecar carrying the node's native-vs-packaged runtime badge. The
# native control surface has no in-process port of compute_runtime_mode, so this
# process computes the value once at startup and persists it here for the front
# to read live. One short word, mode 0640, atomic temp+rename.
RUNTIME_MODE_PATH = ADOS_RUN_DIR / "runtime-mode"

# State IPC reconnect cadence. The native router publishes the snapshot; this
# process subscribes and keeps the connection live across router restarts and
# the cold-start race where the API service comes up before the router has
# created /run/ados/state.sock.
_STATE_IPC_CONNECT_RETRIES = 5
_STATE_IPC_CONNECT_DELAY_S = 1.0
_STATE_IPC_RECONNECT_BACKOFF_S = 2.0


async def _state_ipc_reader(
    state_client: StateIPCClient, shutdown: asyncio.Event, log
) -> None:
    """Keep the state IPC snapshot fresh, reconnecting across router restarts.

    A one-shot connect + read_loop dies permanently the first time the router
    is restarted (read_loop returns, the client is left disconnected) or when
    the API service wins the cold-start race against the router and the socket
    does not exist yet. Either case strands the FC-status snapshot empty, so
    ``/api/command`` 503s forever and ``/api/telemetry`` freezes. This loop
    reconnects with a short backoff and re-drives ``read_loop`` until shutdown.
    """
    while not shutdown.is_set():
        try:
            if not state_client.connected:
                await state_client.connect(
                    retries=_STATE_IPC_CONNECT_RETRIES,
                    delay=_STATE_IPC_CONNECT_DELAY_S,
                )
            await state_client.read_loop()
        except ConnectionError as exc:
            log.debug("state_ipc_connect_failed", error=str(exc))
        except asyncio.CancelledError:
            break
        except Exception as exc:  # noqa: BLE001 — a read error must not kill the reader
            log.warning("state_ipc_read_failed", error=str(exc))
        if shutdown.is_set():
            break
        try:
            await asyncio.wait_for(
                shutdown.wait(), timeout=_STATE_IPC_RECONNECT_BACKOFF_S
            )
            break
        except TimeoutError:
            pass


async def _seed_profile_conf_if_unset(config, log) -> None:
    """Auto-detect the agent profile and record it in profile.conf at boot.

    Runs only for ``agent.profile: auto`` (or unset) on a node whose
    profile.conf names no profile yet; an explicit profile, including
    ``workstation`` and ``compute``, is never overridden by a probe guess.
    Only the ``profile`` key is written; the installer's ``channel`` and
    ``version`` keys are kept. Runs the detection in a worker thread so
    probes never stall the event loop.

    Some probes (i2c, gpio) flake at the moment systemd brings the API
    service up — a transient i2c byte-read on an empty bus can land at
    0x3C and falsely score the node as a ground-station. To defend
    against that, the seed retries after gaps when the probes tie
    (source == "default"). The first attempt waits ~5 s for services
    to settle; subsequent attempts run every 30 s for up to 4 tries
    total. A clean non-tied result on any pass persists and ends the
    loop. If every attempt ties, the seed gives up — the operator can
    pick explicitly via ``ados profile set`` or the setup wizard.
    """
    try:
        explicit = str(getattr(getattr(config, "agent", None), "profile", "") or "")
        if explicit not in ("", "auto"):
            return

        from ados.core.paths import PROFILE_CONF
        from ados.core.profile import _read_profile_conf_value

        if _read_profile_conf_value() is not None:
            return

        from ados.bootstrap.profile_detect import detect_profile, write_profile_conf

        await asyncio.sleep(_PROFILE_SEED_DELAY_S)

        for attempt in range(1, _PROFILE_SEED_MAX_RETRIES + 1):
            # The operator may have set an explicit value (via the
            # wizard or `ados profile set`) between attempts; bail
            # cleanly if the file showed up while we were waiting.
            if _read_profile_conf_value() is not None:
                return

            result = await asyncio.to_thread(detect_profile, None)
            source = str(result.get("source") or "")
            if source != "default":
                ok = await asyncio.to_thread(
                    write_profile_conf, str(result.get("profile") or "")
                )
                if ok:
                    log.info(
                        "profile_conf_seeded",
                        profile=result.get("profile"),
                        source=source,
                        attempt=attempt,
                        path=str(PROFILE_CONF),
                    )
                    # The runtime badge is profile-scoped and was computed at
                    # startup against the pre-seed default; recompute it now.
                    await asyncio.to_thread(_persist_runtime_mode, config, log)
                return
            log.info(
                "profile_seed_tied_retrying",
                attempt=attempt,
                ground_score=result.get("ground_score"),
                air_score=result.get("air_score"),
            )
            if attempt < _PROFILE_SEED_MAX_RETRIES:
                await asyncio.sleep(_PROFILE_SEED_RETRY_GAP_S)

        log.info("profile_seed_gave_up_after_ties", attempts=_PROFILE_SEED_MAX_RETRIES)
    except Exception as exc:  # noqa: BLE001 - boot must never crash on seed
        try:
            log.warning("profile_seed_failed", error=str(exc))
        except Exception:
            pass


def _persist_runtime_mode(config, log) -> None:
    """Compute the node's native-vs-packaged badge once and persist the sidecar.

    The native control surface reads the runtime badge live off this file rather
    than running its own service-binary stat sweep. The value is computed the same
    way the pairing-info and heartbeat paths do — ``compute_runtime_mode`` against
    the wire-contract profile ``current_profile_and_role`` resolves — so the front
    reports the exact same string the FastAPI surface does. Best-effort and atomic
    (temp sibling + rename, mode 0640); a write failure leaves the front to fall
    back to its env / "packaged" default and never crashes startup.
    """
    try:
        import os

        from ados.core.profile import current_profile_and_role
        from ados.core.runtime_mode import compute_runtime_mode

        profile, _role = current_profile_and_role(config)
        mode = compute_runtime_mode(profile)

        RUNTIME_MODE_PATH.parent.mkdir(parents=True, exist_ok=True)
        tmp = RUNTIME_MODE_PATH.with_suffix(".tmp")
        tmp.write_text(mode, encoding="utf-8")
        os.chmod(tmp, 0o640)
        tmp.replace(RUNTIME_MODE_PATH)
        log.info("runtime_mode_persisted", mode=mode, profile=profile)
    except Exception as exc:  # noqa: BLE001 - boot must never crash on the sidecar
        try:
            log.warning("runtime_mode_persist_failed", error=str(exc))
        except Exception:
            pass


async def main() -> int:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("api_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Connect to state IPC for telemetry data. The connect + read is owned by a
    # reconnecting reader task started below so the snapshot survives a router
    # restart or the cold-start race instead of stranding the FC-status surface.
    state_client = StateIPCClient()

    from ados.api.runtime import StandaloneApiRuntime
    from ados.api.server import create_app

    api_runtime = StandaloneApiRuntime(config, state_client, log)
    app = create_app(api_runtime)

    # Persist the native-vs-packaged runtime badge to its sidecar so the native
    # control surface can read it live (it has no in-process port of
    # compute_runtime_mode). Cheap + best-effort; never blocks startup.
    _persist_runtime_mode(config, log)

    # Boot-time profile auto-detect + persist. The full chain depends
    # on /etc/ados/profile.conf to bridge the operator-friendly
    # `agent.profile: auto` default with the wire-contract value the
    # heartbeat reports. Without a one-shot detection here, profile.conf
    # only gets written when a setup-webapp client polls /api/setup/status.
    # Fire-and-forget so a slow probe never blocks API startup.
    profile_seed = asyncio.create_task(
        _seed_profile_conf_if_unset(config, log),
        name="profile-seed",
    )

    # The only listener is the internal Unix socket the native front proxies
    # to. This app has no auth layer of its own, so it never binds TCP.
    from ados.api.internal_socket import bind_internal_socket, internal_socket_path

    socket_path = internal_socket_path()
    sockets = [bind_internal_socket(socket_path)]
    uvi_config = uvicorn.Config(
        app,
        log_level="warning",
        access_log=False,
    )
    server = uvicorn.Server(uvi_config)

    serve_task = asyncio.create_task(server.serve(sockets=sockets), name="uvicorn")
    tasks = [
        serve_task,
        asyncio.create_task(
            _state_ipc_reader(state_client, shutdown, log),
            name="state-reader",
        ),
        profile_seed,
    ]

    log.info("api_service_ready", socket=str(socket_path))

    # Wait for a shutdown signal, or for the HTTP server to die on its own (a
    # bind failure, an unhandled startup error). A process left running with
    # no listener would read active to systemd and never be restarted.
    shutdown_wait = asyncio.create_task(shutdown.wait(), name="shutdown-wait")
    await asyncio.wait({shutdown_wait, serve_task}, return_when=asyncio.FIRST_COMPLETED)
    server_died = not shutdown.is_set()
    if server_died:
        exc = None if serve_task.cancelled() else serve_task.exception()
        log.error("api_server_exited", error=str(exc) if exc else None)
    shutdown_wait.cancel()

    log.info("api_service_stopping")
    server.should_exit = True
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await state_client.disconnect()
    log.info("api_service_stopped")
    return 1 if server_died else 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        sys.exit(0)
