"""MQTT gateway — publishes telemetry and status, subscribes to commands."""

from __future__ import annotations

import asyncio
import json
import ssl
from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.core.config import ADOSConfig
    from ados.services.mavlink.ipc_state import IpcVehicleState as VehicleState

log = get_logger("mqtt")

# Fixed pause between broker connect attempts. The gateway never gives up: a
# broker that is down at boot is reached as soon as it comes back.
CONNECT_RETRY_S = 3.0


class MqttGateway:
    """MQTT client that bridges vehicle state to cloud/self-hosted broker."""

    def __init__(self, config: ADOSConfig, state: VehicleState, api_key: str | None = None) -> None:
        self.config = config
        self.state = state
        self._client = None
        self._device_id = config.agent.device_id
        self._api_key = api_key  # From pairing, used as MQTT password in cloud mode

    def _credentials(self) -> tuple[str, str | None]:
        """The broker username and password.

        The username is the bare device id unless explicitly overridden, so the
        broker ACL pattern ``ados/%u/#`` resolves to this agent's own topic
        subtree ``ados/<device_id>/...``. In cloud mode the pairing key is the
        password when none is configured.
        """
        server = self.config.server
        user = server.mqtt_username or self._device_id
        password = server.mqtt_password
        if server.mode == "cloud" and self._api_key:
            password = password or self._api_key
        return user, password or None

    async def _connect(self, client, broker: str, port: int, shutdown: asyncio.Event) -> bool:
        """Connect, retrying on a fixed interval until connected or shut down."""
        while not shutdown.is_set():
            try:
                await asyncio.to_thread(client.connect, broker, port, 60)
                return True
            except Exception as e:  # noqa: BLE001 — DNS, refused, TLS, auth: all retried
                log.warning("mqtt_connect_failed", broker=broker, error=str(e), retry_s=CONNECT_RETRY_S)
            try:
                await asyncio.wait_for(shutdown.wait(), timeout=CONNECT_RETRY_S)
            except TimeoutError:
                pass
        return False

    def _get_broker_config(self) -> tuple[str, int]:
        """Get broker host and port based on server mode."""
        if self.config.server.mode == "local":
            # Local-only operators don't want a cloud MQTT round-trip.
            # `run()` short-circuits on an empty broker, so the gateway
            # stays quiet without disabling the rest of the agent.
            return ("", 0)
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
        # One ClientID per broker lane, never per device. MQTT requires the
        # broker to disconnect the existing session when a second client
        # presents the same ClientID, and the native MAVLink relay holds the
        # bare `ados-{device_id}` — sharing it made the two evict each other in
        # a sub-second loop forever (no cloud telemetry, no cloud command
        # authority, a flapping mqttConnected). The suffixed lanes are
        # `-msp`, `-atlas`, `-vision` and this one, `-gw`.
        client = mqtt.Client(
            client_id=f"ados-{self._device_id}-gw",
            callback_api_version=mqtt.CallbackAPIVersion.VERSION2,
            transport=transport,
        )
        # Higher inflight ceiling avoids drops at telemetry burst rates.
        client.max_inflight_messages_set(1000)

        # WebSocket path (Mosquitto default)
        if transport == "websockets":
            client.ws_set_options(path="/mqtt")

        user, password = self._credentials()
        client.username_pw_set(user, password)

        # TLS on every transport, verified against the system trust store: the
        # password is the pairing key. Only the explicit dev flag turns it off,
        # and a TLS setup failure stops the gateway rather than falling back
        # to plaintext.
        if self.config.server.mqtt_plaintext_dev:
            log.warning("mqtt_plaintext_dev", reason="server.mqtt_plaintext_dev is set")
        else:
            try:
                client.tls_set(cert_reqs=ssl.CERT_REQUIRED)
            except Exception as e:  # noqa: BLE001 — any TLS setup failure is fatal here
                log.error("mqtt_tls_setup_failed", error=str(e))
                return

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

        # Subscribing on every CONNACK keeps the command subscription across the
        # client's own automatic reconnects.
        def on_connect(client, userdata, flags, reason_code, properties):
            client.subscribe(f"ados/{self._device_id}/command")

        client.on_message = on_message
        client.on_connect = on_connect

        if not await self._connect(client, broker, port, shutdown):
            return
        client.loop_start()
        log.info("mqtt_connected", broker=broker, port=port)

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

                # Publish status. fc_connected is the router's GATED truth from
                # the state snapshot (transport open AND a fresh HEARTBEAT), NOT
                # `bool(last_heartbeat)` — that ISO string is non-empty forever
                # after the first heartbeat ever decoded, so it reported connected
                # long after the link went dead. "Presence is not proof."
                status = {
                    "device_id": self._device_id,
                    "name": self.config.agent.name,
                    "tier": self.config.agent.tier,
                    "armed": self.state.armed,
                    "fc_connected": bool(self.state.snapshot.get("fc_connected", False)),
                    "mavlink_alive": bool(self.state.snapshot.get("mavlink_alive", False)),
                    "heartbeat_age_s": self.state.snapshot.get("heartbeat_age_s"),
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
