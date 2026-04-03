"""MAVLink frame relay over MQTT for remote GCS access.

Bridges raw MAVLink frames between the local MAVLink IPC socket and
MQTT topics, enabling browser-based GCS to communicate with the
flight controller from anywhere.

Topics:
  ados/{device_id}/mavlink/tx — FC→GCS (agent publishes, GCS subscribes)
  ados/{device_id}/mavlink/rx — GCS→FC (GCS publishes, agent subscribes)
"""

from __future__ import annotations

import asyncio
import ssl

import paho.mqtt.client as mqtt_client
import structlog

from ados.core.ipc import MavlinkIPCClient

log = structlog.get_logger("cloud.mavlink_relay")


class MavlinkMqttRelay:
    """Relays raw MAVLink frames between IPC socket and MQTT broker."""

    def __init__(
        self,
        device_id: str,
        broker: str,
        port: int,
        transport: str = "websockets",
        username: str = "",
        password: str = "",
    ) -> None:
        self._device_id = device_id
        self._broker = broker
        self._port = port
        self._transport = transport
        self._username = username
        self._password = password
        self._topic_tx = f"ados/{device_id}/mavlink/tx"
        self._topic_rx = f"ados/{device_id}/mavlink/rx"
        self._ipc: MavlinkIPCClient | None = None
        self._mqtt: mqtt_client.Client | None = None
        self._running = False

    async def start(self, shutdown: asyncio.Event) -> None:
        """Connect to IPC + MQTT and relay frames until shutdown."""
        # Set up MQTT client
        client_id = f"ados-mavlink-{self._device_id}"
        self._mqtt = mqtt_client.Client(
            client_id=client_id,
            transport=self._transport,
            protocol=mqtt_client.MQTTv5,
        )

        if self._username:
            self._mqtt.username_pw_set(self._username, self._password)

        if self._transport == "websockets":
            self._mqtt.tls_set(cert_reqs=ssl.CERT_NONE)
            self._mqtt.tls_insecure_set(True)
            self._mqtt.ws_set_options(path="/mqtt")

        # GCS→FC: forward MQTT messages to IPC
        def on_message(_client, _userdata, msg):
            if self._ipc and msg.payload:
                try:
                    self._ipc.send(msg.payload)
                except Exception:
                    pass

        def on_connect(_client, _userdata, _flags, _reason, _properties=None):
            log.info("mavlink_relay_mqtt_connected", broker=self._broker)
            self._mqtt.subscribe(self._topic_rx, qos=0)

        self._mqtt.on_message = on_message
        self._mqtt.on_connect = on_connect

        try:
            self._mqtt.connect(self._broker, self._port, keepalive=60)
        except Exception as e:
            log.error("mavlink_relay_mqtt_connect_failed", error=str(e))
            return

        self._mqtt.loop_start()

        # Connect to MAVLink IPC socket
        self._ipc = MavlinkIPCClient()
        try:
            await self._ipc.connect(retries=10, delay=2.0)
        except ConnectionError as e:
            log.warning("mavlink_relay_ipc_unavailable", error=str(e))
            self._mqtt.loop_stop()
            self._mqtt.disconnect()
            return

        # FC→MQTT: forward IPC frames to MQTT
        def on_ipc_data(data: bytes) -> None:
            if self._mqtt and self._mqtt.is_connected():
                self._mqtt.publish(self._topic_tx, data, qos=0)

        self._ipc.set_data_handler(on_ipc_data)
        self._running = True
        log.info(
            "mavlink_relay_started",
            device_id=self._device_id,
            topic_tx=self._topic_tx,
            topic_rx=self._topic_rx,
        )

        # Run IPC read loop until shutdown or disconnect
        try:
            read_task = asyncio.create_task(self._ipc.read_loop())
            shutdown_task = asyncio.create_task(shutdown.wait())
            done, pending = await asyncio.wait(
                [read_task, shutdown_task],
                return_when=asyncio.FIRST_COMPLETED,
            )
            for task in pending:
                task.cancel()
        except Exception as e:
            log.error("mavlink_relay_error", error=str(e))
        finally:
            await self.stop()

    async def stop(self) -> None:
        """Disconnect IPC and MQTT."""
        self._running = False
        if self._ipc:
            await self._ipc.disconnect()
            self._ipc = None
        if self._mqtt:
            self._mqtt.loop_stop()
            self._mqtt.disconnect()
            self._mqtt = None
        log.info("mavlink_relay_stopped")
