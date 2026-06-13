"""Standalone REST API service.

Runs the FastAPI server with uvicorn, connecting to state IPC for live
telemetry data on status endpoints.

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

_PROFILE_SEED_DELAY_S = 5.0
_PROFILE_SEED_RETRY_GAP_S = 30.0
_PROFILE_SEED_MAX_RETRIES = 4

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
    """Auto-detect the agent profile and persist to profile.conf at boot.

    No-op when ``agent.profile`` is already an explicit value or when
    profile.conf is already populated. Runs the detection in a worker
    thread so probes never stall the event loop.

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
        if explicit in ("drone", "ground_station"):
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
                ok = await asyncio.to_thread(write_profile_conf, result)
                if ok:
                    log.info(
                        "profile_conf_seeded",
                        profile=result.get("profile"),
                        source=source,
                        attempt=attempt,
                        path=str(PROFILE_CONF),
                    )
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


async def main() -> None:
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

    # Boot-time profile auto-detect + persist. The full chain depends
    # on /etc/ados/profile.conf to bridge the operator-friendly
    # `agent.profile: auto` default with the wire-contract value the
    # heartbeat reports. Without a one-shot detection here, profile.conf
    # only gets written when a setup-webapp client polls /api/setup/status.
    # Fire-and-forget so a slow probe never blocks API startup.
    asyncio.create_task(
        _seed_profile_conf_if_unset(config, log),
        name="profile-seed",
    )

    api_config = config.api.rest
    # Bind explicit AF_INET + AF_INET6 sockets so both IPv4 and IPv6
    # clients reach the agent regardless of which family the browser's
    # mDNS resolver returns first. uvicorn alone with `host="::"` did
    # not produce a working IPv4 listener on some Pi kernels. When the
    # native front owns the LAN port, ADOS_API_INTERNAL_SOCKET redirects
    # this to a single Unix socket the front proxies to instead.
    from ados.api.dual_bind import make_listen_sockets
    sockets = make_listen_sockets(api_config.host, api_config.port)
    uvi_config = uvicorn.Config(
        app,
        log_level="warning",
        access_log=False,
    )
    server = uvicorn.Server(uvi_config)

    tasks = [
        asyncio.create_task(server.serve(sockets=sockets), name="uvicorn"),
        asyncio.create_task(
            _state_ipc_reader(state_client, shutdown, log),
            name="state-reader",
        ),
    ]

    log.info("api_service_ready", host=api_config.host, port=api_config.port)

    # Wait for shutdown signal
    await shutdown.wait()

    log.info("api_service_stopping")
    server.should_exit = True
    for task in tasks:
        task.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    await state_client.disconnect()
    log.info("api_service_stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
