"""MAVLink + WebRTC relay supervisor loops.

These loops sit alongside the heartbeat and command-poll loops in the
cloud subprocess. They block while unpaired (cheap 5 s sleep), then
spin up the matching relay client. The relay client's ``start()``
runs until the agent unpairs or the supervisor signals shutdown.
On any exit (clean or exception) the supervisor sleeps 5 s and
retries — this is the canonical reconnect pattern for the cloud
relay clients.
"""

from __future__ import annotations

import asyncio

from ._context import CloudContext


async def mavlink_relay_task(ctx: CloudContext) -> None:
    """Relay raw MAVLink frames over MQTT for remote GCS access."""
    config = ctx.config
    pairing = ctx.pairing
    shutdown = ctx.shutdown
    log = ctx.log

    while not shutdown.is_set():
        if not pairing.is_paired:
            await asyncio.sleep(5)
            continue
        try:
            from ados.services.cloud.mavlink_relay import MavlinkMqttRelay

            relay = MavlinkMqttRelay(
                device_id=config.agent.device_id,
                broker=config.server.cloud.mqtt_broker,
                port=config.server.cloud.mqtt_port,
                transport=config.server.mqtt_transport,
                username=f"ados-{config.agent.device_id}",
                password=pairing.api_key or "",
            )
            await relay.start(shutdown)
        except Exception as exc:
            log.warning("mavlink_relay_failed", error=str(exc))
            await asyncio.sleep(5)


async def webrtc_signaling_task(ctx: CloudContext) -> None:
    """Relay WebRTC SDP offers/answers over MQTT for cross-network video.

    Browser dials in from command.altnautica.com on any network; SDP
    handshake flows via MQTT, media flows direct peer-to-peer after
    ICE punching.
    """
    config = ctx.config
    pairing = ctx.pairing
    shutdown = ctx.shutdown
    log = ctx.log

    while not shutdown.is_set():
        if not pairing.is_paired:
            await asyncio.sleep(5)
            continue
        try:
            from ados.services.cloud.webrtc_signaling import WebrtcSignalingRelay

            relay = WebrtcSignalingRelay(
                device_id=config.agent.device_id,
                broker=config.server.cloud.mqtt_broker,
                port=config.server.cloud.mqtt_port,
                transport=config.server.mqtt_transport,
                username=f"ados-{config.agent.device_id}",
                password=pairing.api_key or "",
            )
            await relay.start(shutdown)
        except Exception as exc:
            log.warning("webrtc_signaling_failed", error=str(exc))
            await asyncio.sleep(5)


__all__ = ["mavlink_relay_task", "webrtc_signaling_task"]
