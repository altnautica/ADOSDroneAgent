"""MQTT gateway — publishes telemetry and status, subscribes to commands."""

from __future__ import annotations

import asyncio
import json
from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.core.config import ADOSConfig
    from ados.services.mavlink.state import VehicleState

log = get_logger("mqtt")


class MqttGateway:
    """MQTT client that bridges vehicle state to cloud/self-hosted broker."""

    def __init__(self, config: ADOSConfig, state: VehicleState) -> None:
        self.config = config
        self.state = state
        self._client = None
        self._device_id = config.agent.device_id

    def _get_broker_config(self) -> tuple[str, int]:
        """Get broker host and port based on server mode."""
        if self.config.server.mode == "self_hosted":
            return (
                self.config.server.self_hosted.mqtt_broker,
                self.config.server.self_hosted.mqtt_port,
            )
        return (
            self.config.server.cloud.mqtt_broker,
            self.config.server.cloud.mqtt_port,
        )

    async def run(self, shutdown: asyncio.Event) -> None:
        """Main MQTT loop — connect, publish, subscribe."""
        broker, port = self._get_broker_config()
        if not broker:
            log.info("mqtt_disabled", reason="no broker configured")
            return

        try:
            import paho.mqtt.client as mqtt
        except ImportError:
            log.warning("mqtt_unavailable", reason="paho-mqtt not installed")
            return

        client = mqtt.Client(
            client_id=f"ados-{self._device_id}",
            callback_api_version=mqtt.CallbackAPIVersion.VERSION2,
        )

        # TLS if enabled
        if self.config.security.tls.enabled:
            try:
                client.tls_set(
                    ca_certs=self.config.security.tls.ca_path,
                    certfile=self.config.security.tls.cert_path,
                    keyfile=self.config.security.tls.key_path,
                )
            except Exception as e:
                log.warning("mqtt_tls_failed", error=str(e))

        # Command handler
        def on_message(client, userdata, msg):
            try:
                payload = json.loads(msg.payload.decode())
                log.info("mqtt_command", topic=msg.topic, payload=payload)
            except Exception as e:
                log.warning("mqtt_parse_error", error=str(e))

        client.on_message = on_message

        # Connect
        try:
            client.connect(broker, port, keepalive=60)
            client.subscribe(f"ados/{self._device_id}/command")
            client.loop_start()
            log.info("mqtt_connected", broker=broker, port=port)
        except Exception as e:
            log.error("mqtt_connect_failed", broker=broker, error=str(e))
            return

        self._client = client
        rate = self.config.server.telemetry_rate
        interval = 1.0 / rate if rate > 0 else 1.0

        try:
            while not shutdown.is_set():
                # Publish telemetry
                telemetry = self.state.to_dict()
                client.publish(
                    f"ados/{self._device_id}/telemetry",
                    json.dumps(telemetry),
                    qos=0,
                )

                # Publish status
                status = {
                    "device_id": self._device_id,
                    "name": self.config.agent.name,
                    "tier": self.config.agent.tier,
                    "armed": self.state.armed,
                    "fc_connected": bool(self.state.last_heartbeat),
                }
                client.publish(
                    f"ados/{self._device_id}/status",
                    json.dumps(status),
                    qos=1,
                )

                await asyncio.sleep(interval)
        finally:
            client.loop_stop()
            client.disconnect()
            log.info("mqtt_disconnected")
