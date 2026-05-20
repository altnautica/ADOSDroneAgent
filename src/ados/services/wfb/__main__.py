"""Standalone WFB-ng link manager service.

Detects compatible WiFi adapters, sets monitor mode, and manages wfb_tx/wfb_rx
subprocesses with auto-restart and link quality monitoring.

Run: python -m ados.services.wfb
"""

from __future__ import annotations

import asyncio
import signal
import sys

import structlog

from ados.core.config import load_config
from ados.core.logging import configure_logging


async def main() -> None:
    config = load_config()
    configure_logging(config.logging.level)
    log = structlog.get_logger()
    log.info("wfb_service_starting")

    shutdown = asyncio.Event()
    loop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, shutdown.set)

    # Gate the drone-side WfbManager on profile. On a ground-station
    # node, the dedicated `ados-wfb-rx` unit owns the radio and writes
    # /run/ados/wfb-stats.json. If we also start WfbManager here, the
    # two writers race for that file and the GS-side stats are clobbered
    # by the drone-side manager's "blocked_unpaired" idle state — the
    # LCD ends up reading channel 40 / 0 packets even when wfb_rx is
    # actually decoding at full rate.
    from ados.core.profile import current_profile_and_role as _resolve_profile
    _wire_profile, _ = _resolve_profile(config)
    is_ground = _wire_profile == "ground-station"

    from ados.services.wfb.manager import WfbManager

    manager: WfbManager | None = None
    manager_task: asyncio.Task | None = None
    if is_ground:
        log.info(
            "wfb_service_idle_on_ground",
            msg="dedicated ados-wfb-rx owns the radio; ados-wfb stays idle",
        )
    else:
        manager = WfbManager(config.video.wfb)
        # Run the WFB manager (handles adapter detection, monitor mode, process lifecycle)
        manager_task = asyncio.create_task(manager.run(), name="wfb-manager")

    # NOTE: the auto_pair supervisor is hosted in ados-cloud, not here.
    # The bind orchestrator stops + starts ados-wfb to flip profiles, so
    # if auto_pair ran here the orchestrator would kill its own host.

    # Sibling tasks bound to the wfb manager's lifetime.
    sibling_tasks: list[asyncio.Task] = []

    # Closed-loop bitrate + FEC controller. Off by default
    # (WfbConfig.adaptive_bitrate_enabled = false); the controller
    # still runs disabled so its snapshot surface is populated for
    # /api/video/config consumers. Requires the drone-side WfbManager
    # to drive its link_quality_monitor + set_fec, so it only wires
    # up when the manager is alive.
    if manager is not None:
        try:
            from ados.services.video.bitrate_controller import BitrateController

            controller = BitrateController(
                link_quality_monitor=manager.monitor,
                set_fec=manager.set_fec,
                # Bitrate restart goes through the video pipeline which
                # lives in ados-video. We expose a no-op so the
                # controller's logic is exercised but the encoder restart
                # is deferred until a future cross-process bridge ships.
                # The intent today is to surface the controller's
                # diagnostic state, not to actually adapt bitrate.
                set_bitrate=_set_bitrate_noop,
                enabled=config.video.wfb.adaptive_bitrate_enabled,
            )
            sibling_tasks.append(
                asyncio.create_task(controller.run(), name="bitrate-controller")
            )
            log.info(
                "bitrate_controller_wired",
                enabled=config.video.wfb.adaptive_bitrate_enabled,
            )
        except Exception as exc:  # noqa: BLE001
            log.warning("bitrate_controller_wire_skipped", error=str(exc))

    # Coordinated frequency-hopping supervisor on the drone side only.
    # The GS-side counterpart spawns inside ground_station/wfb_rx.run().
    # Gated on auto_hop_enabled so a fixed-frequency deployment opts
    # out by flipping a single flag. Also requires the WfbManager to
    # be alive (skipped above on ground profile).
    if (
        manager is not None
        and not is_ground
        and getattr(config.video.wfb, "auto_hop_enabled", True)
    ):
        try:
            from ados.services.wfb.hop_supervisor import HopSupervisor

            supervisor = HopSupervisor(
                wfb_manager=manager,
                link_quality_monitor=manager.monitor,
                band=getattr(config.video.wfb, "band", "u-nii-1"),
                hop_period_seconds=int(
                    getattr(config.video.wfb, "hop_period_seconds", 60)
                ),
                loss_threshold_percent=float(
                    getattr(
                        config.video.wfb,
                        "hop_loss_threshold_percent",
                        10.0,
                    )
                ),
                rssi_threshold_dbm=float(
                    getattr(
                        config.video.wfb,
                        "hop_rssi_threshold_dbm",
                        -75.0,
                    )
                ),
                enabled=True,
            )
            sibling_tasks.append(
                asyncio.create_task(supervisor.run(), name="hop-supervisor")
            )
            log.info(
                "hop_supervisor_wired",
                band=getattr(config.video.wfb, "band", "u-nii-1"),
                period=getattr(config.video.wfb, "hop_period_seconds", 60),
            )
        except Exception as exc:  # noqa: BLE001
            log.warning("hop_supervisor_wire_skipped", error=str(exc))

    log.info("wfb_service_ready", profile=config.agent.profile)

    # Wait for shutdown
    await shutdown.wait()

    log.info("wfb_service_stopping")
    for t in sibling_tasks:
        t.cancel()
    if manager_task is not None:
        manager_task.cancel()
        await asyncio.gather(
            manager_task, *sibling_tasks, return_exceptions=True,
        )
        if manager is not None:
            await manager.stop()
    else:
        await asyncio.gather(*sibling_tasks, return_exceptions=True)
    log.info("wfb_service_stopped")


async def _set_bitrate_noop(_kbps: int) -> bool:
    """Placeholder bitrate setter for the wfb-side controller.

    The actual encoder restart lives in ados-video's VideoPipeline.
    Until a cross-process bridge is wired, the wfb-side controller
    only mutates FEC (which it CAN do directly) and reports the
    intended bitrate in its snapshot. Returns True so the controller
    treats the step as applied for its hysteresis bookkeeping; the
    operator-visible UI surface will still show the recommended
    bitrate even when the encoder doesn't follow.
    """
    return True


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
    sys.exit(0)
