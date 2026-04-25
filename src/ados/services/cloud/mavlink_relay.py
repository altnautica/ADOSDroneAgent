"""MAVLink frame relay over MQTT for remote GCS access.

Bridges raw MAVLink frames between the local MAVLink IPC socket and
MQTT topics, enabling browser-based GCS to communicate with the
flight controller from anywhere.

Topics:
  ados/{device_id}/mavlink/tx — FC→GCS (agent publishes, GCS subscribes)
  ados/{device_id}/mavlink/rx — GCS→FC (GCS publishes, agent subscribes)

Architecture note:
  Uses an asyncio.Queue (maxsize=2000) + drop-oldest policy + a
  dedicated publisher coroutine to decouple the IPC reader from paho's
  publish path.

  Root cause this design avoids: a synchronous per-frame publish
  callback would let paho publish() block when the broker or tunnel
  is slow, stall the IPC reader, push back through the kernel TCP
  buffer, stop the serial FC read, overrun the FC transmit buffer,
  and produce a visible telemetry freeze in the GCS within 10-30s
  of any slow link.

  Fix: on_ipc_data does put_nowait() into a bounded queue, with
  drop-oldest fallback on QueueFull (recency beats completeness). A
  separate _publish_loop coroutine drains the queue and publishes via
  paho. Throughput metrics (frames_in / frames_published /
  frames_dropped_queue_full / frames_dropped_not_connected /
  publish_errors) are logged every 10s.
"""

from __future__ import annotations

import asyncio
import ssl

import paho.mqtt.client as mqtt_client
import structlog

from ados.core.ipc import MavlinkIPCClient

log = structlog.get_logger("cloud.mavlink_relay")

# Queue + metric constants. 2000 frames at 30 msg/s gives ~66 seconds
# of headroom. Earlier 200-frame size produced 17.4% sustained drop
# rate over 30+ minutes on a Cloudflare WSS tunnel. Root cause was
# paho-mqtt's default max_inflight_messages=20: paho's internal queue
# fills almost instantly when the round-trip is anything above ~50ms.
# Bumping the asyncio queue alone is not enough, so we ALSO bump
# paho's inflight limit below in the Client constructor.
_QUEUE_MAXSIZE = 2000  # ~66s at 30 msg/s before drops start
_METRIC_LOG_INTERVAL = 10.0  # seconds
# paho's default max_inflight is 20, way too low for the SpeedyBee
# F405's ~30 msg/s rate over a Cloudflare WSS tunnel. Bump to 1000 so
# the actual MQTT publish path is the limit, not paho's internal
# queue. paho honors this even at QoS 0 because it tracks all in-flight
# publishes through its socket buffer.
_PAHO_MAX_INFLIGHT = 1000


class MavlinkMqttRelay:
    """Relays raw MAVLink frames between IPC socket and MQTT broker.

    Architecture:

        IPC reader -> on_ipc_data -> put_nowait -> asyncio.Queue -+
                                       (drop oldest on full)     |
                                                                 v
                                              _publish_loop coroutine
                                                       |
                                                       v
                                                paho publish()

    Decouples the IPC read path from paho's internal queue, so broker or
    tunnel slowness cannot backpressure the serial FC read.
    """

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

        # Async queue + publisher coroutine state.
        self._queue: asyncio.Queue[bytes] | None = None
        self._publish_task: asyncio.Task | None = None
        self._metrics: dict[str, int] = {
            "frames_in": 0,
            "frames_published": 0,
            "frames_dropped_queue_full": 0,
            "frames_dropped_not_connected": 0,
            "publish_errors": 0,
        }
        self._last_metric_log: float = 0.0

    async def start(self, shutdown: asyncio.Event) -> None:
        """Connect to IPC + MQTT and relay frames until shutdown."""
        # Set up MQTT client
        client_id = f"ados-mavlink-{self._device_id}"
        self._mqtt = mqtt_client.Client(
            client_id=client_id,
            transport=self._transport,
            protocol=mqtt_client.MQTTv5,
        )

        # Bump paho's max in-flight messages from default 20 to 1000.
        # The default was the actual bottleneck causing 17.4% drops on
        # the bench: paho silently throttles publish() once the
        # inflight counter hits this limit, and at 30 msg/s with
        # WSS/Cloudflare RTT of ~50-150ms, the inflight queue fills
        # almost instantly.
        self._mqtt.max_inflight_messages_set(_PAHO_MAX_INFLIGHT)

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

        # Create queue + start publisher coroutine.
        self._queue = asyncio.Queue(maxsize=_QUEUE_MAXSIZE)
        self._running = True
        self._publish_task = asyncio.create_task(self._publish_loop())

        # Connect to MAVLink IPC socket
        self._ipc = MavlinkIPCClient()
        try:
            await self._ipc.connect(retries=10, delay=2.0)
        except ConnectionError as e:
            log.warning("mavlink_relay_ipc_unavailable", error=str(e))
            self._running = False
            if self._publish_task:
                self._publish_task.cancel()
            self._mqtt.loop_stop()
            self._mqtt.disconnect()
            return

        # FC->MQTT: enqueue frames for the publisher coroutine.
        # Drop-oldest on QueueFull preserves recency.
        def on_ipc_data(data: bytes) -> None:
            self._metrics["frames_in"] += 1
            if self._queue is None:
                return
            try:
                self._queue.put_nowait(data)
            except asyncio.QueueFull:
                try:
                    _ = self._queue.get_nowait()
                    self._metrics["frames_dropped_queue_full"] += 1
                    self._queue.put_nowait(data)
                except (asyncio.QueueEmpty, asyncio.QueueFull):
                    pass

        self._ipc.set_data_handler(on_ipc_data)
        log.info(
            "mavlink_relay_started",
            device_id=self._device_id,
            topic_tx=self._topic_tx,
            topic_rx=self._topic_rx,
            queue_maxsize=_QUEUE_MAXSIZE,
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

    async def _publish_loop(self) -> None:
        """Drain the queue and publish to MQTT.

        Runs as a separate asyncio task so paho publish blocking cannot
        stall the IPC reader. Logs throughput metrics every 10s.
        """
        log.info("mavlink_relay_publish_loop_started")
        loop = asyncio.get_event_loop()
        self._last_metric_log = loop.time()

        while self._running:
            try:
                try:
                    # 1s timeout lets us check metrics even on idle link
                    data = await asyncio.wait_for(
                        self._queue.get(), timeout=1.0
                    )
                except asyncio.TimeoutError:
                    self._maybe_log_metrics(loop.time())
                    continue

                try:
                    if self._mqtt and self._mqtt.is_connected():
                        self._mqtt.publish(self._topic_tx, data, qos=0)
                        self._metrics["frames_published"] += 1
                    else:
                        self._metrics["frames_dropped_not_connected"] += 1
                except Exception as e:
                    self._metrics["publish_errors"] += 1
                    if self._metrics["publish_errors"] <= 5:
                        log.warning("mavlink_publish_error", error=str(e))

                self._maybe_log_metrics(loop.time())

            except asyncio.CancelledError:
                log.info("mavlink_relay_publish_loop_cancelled")
                break
            except Exception as e:
                log.error("mavlink_relay_publish_loop_error", error=str(e))
                await asyncio.sleep(0.1)

        log.info("mavlink_relay_publish_loop_stopped", **self._metrics)

    def _maybe_log_metrics(self, now: float) -> None:
        """Log throughput metrics periodically."""
        if now - self._last_metric_log >= _METRIC_LOG_INTERVAL:
            log.info("mavlink_relay_metrics", **self._metrics)
            self._last_metric_log = now

    async def stop(self) -> None:
        """Disconnect IPC and MQTT."""
        self._running = False

        # Cancel the publisher coroutine.
        if self._publish_task:
            self._publish_task.cancel()
            try:
                await self._publish_task
            except (asyncio.CancelledError, Exception):
                pass
            self._publish_task = None

        if self._ipc:
            try:
                await self._ipc.disconnect()
            except Exception:
                pass
            self._ipc = None
        if self._mqtt:
            try:
                self._mqtt.loop_stop()
                self._mqtt.disconnect()
            except Exception:
                pass
            self._mqtt = None
        log.info("mavlink_relay_stopped", **self._metrics)
