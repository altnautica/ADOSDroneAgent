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

    def __init__(self, config: ADOSConfig, state: VehicleState, api_key: str | None = None) -> None:
        self.config = config
        self.state = state
        self._client = None
        self._device_id = config.agent.device_id
        self._api_key = api_key  # From pairing, used as MQTT password in cloud mode

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

        # Configure transport
        transport = self.config.server.mqtt_transport
        client = mqtt.Client(
            client_id=f"ados-{self._device_id}",
            callback_api_version=mqtt.CallbackAPIVersion.VERSION2,
            transport=transport,
        )
        # Higher inflight ceiling avoids drops at telemetry burst rates.
        client.max_inflight_messages_set(1000)

        # WebSocket path (Mosquitto default)
        if transport == "websockets":
            client.ws_set_options(path="/mqtt")

        # Username/password auth — in cloud mode, auto-use deviceId + apiKey
        mqtt_user = self.config.server.mqtt_username
        mqtt_pass = self.config.server.mqtt_password
        if self.config.server.mode == "cloud" and self._api_key:
            mqtt_user = mqtt_user or f"ados-{self._device_id}"
            mqtt_pass = mqtt_pass or self._api_key
        if mqtt_user:
            client.username_pw_set(mqtt_user, mqtt_pass)

        # TLS for secure connections
        if self.config.security.tls.enabled or transport == "websockets":
            try:
                import ssl
                client.tls_set(cert_reqs=ssl.CERT_NONE)
                # TLS insecure is safe here: Cloudflare Tunnel terminates TLS
                # at the edge. The connection between agent and Cloudflare is
                # encrypted, but the tunnel endpoint uses a self-signed cert.
                client.tls_insecure_set(True)
            except Exception as e:
                log.warning("mqtt_tls_failed", error=str(e))

        # Use port 443 for WebSocket (through Cloudflare Tunnel)
        if transport == "websockets" and port == 8883:
            port = 443

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
                await asyncio.to_thread(
                    client.publish,
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
                await asyncio.to_thread(
                    client.publish,
                    f"ados/{self._device_id}/status",
                    json.dumps(status),
                    qos=1,
                )

                await asyncio.sleep(interval)
        finally:
            client.loop_stop()
            client.disconnect()
            log.info("mqtt_disconnected")
