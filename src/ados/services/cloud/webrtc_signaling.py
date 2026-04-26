"""WebRTC SDP signaling relay over MQTT.

Lets a browser at command.altnautica.com (or any HTTPS origin)
establish a WebRTC peer connection to mediamtx running on this agent
EVEN WHEN THE BROWSER IS ON A DIFFERENT NETWORK from the SBC.

The actual video bytes flow direct peer-to-peer via WebRTC after the
SDP handshake. This module ONLY handles the signaling exchange (~5-10 KB
of SDP text per session start). The signaling round-trip is sub-second.

Architecture:

  Browser publishes SDP offer → ados/{device_id}/webrtc/offer
            ↓
  This relay subscribes to that topic, receives the offer
            ↓
  POSTs the offer to local mediamtx WHEP at http://localhost:8889/main/whep
            ↓
  mediamtx generates an SDP answer (containing both LAN host candidate
  AND STUN-discovered srflx public candidate)
            ↓
  Relay publishes the answer → ados/{device_id}/webrtc/answer
            ↓
  Browser receives the answer, calls setRemoteDescription, ICE punches
  through the NAT via the STUN candidates, media flows direct.

Modeled on MavlinkMqttRelay (services/cloud/mavlink_relay.py).
"""

from __future__ import annotations

import asyncio
import json
import ssl
import time

import httpx
import paho.mqtt.client as mqtt_client
import structlog

log = structlog.get_logger("cloud.webrtc_signaling")

# Local mediamtx WHEP endpoint that the agent's video service exposes.
# mediamtx is started by ados-video and listens on the SBC's loopback at
# port 8889 (configured in services/video/mediamtx.py). The path /main/whep
# is the WHEP endpoint for the "main" stream (the camera feed).
_LOCAL_WHEP_URL = "http://localhost:8889/main/whep"

# WHEP POST timeout. mediamtx normally answers in <100ms; 5s gives plenty
# of headroom for first-load when mediamtx is warming up.
_WHEP_TIMEOUT_SEC = 5.0

# Periodic metric log interval. Mostly debug — signaling is so low-volume
# that there's not much to track other than offer count.
_METRIC_LOG_INTERVAL = 60.0


class WebrtcSignalingRelay:
    """Relays WebRTC SDP offers/answers between MQTT and local mediamtx.

    The signaling channel is two MQTT topics scoped to the device_id:
      - ados/{device_id}/webrtc/offer  (browser → agent, SDP offer)
      - ados/{device_id}/webrtc/answer (agent → browser, SDP answer)

    On each offer received, we POST it to mediamtx's local WHEP endpoint
    and publish the resulting SDP answer back via MQTT. The actual video
    bytes flow peer-to-peer via WebRTC after this handshake — this module
    is only the rendezvous mechanism.
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
        self._topic_offer = f"ados/{device_id}/webrtc/offer"
        self._topic_answer = f"ados/{device_id}/webrtc/answer"
        self._mqtt: mqtt_client.Client | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._http: httpx.AsyncClient | None = None
        self._metrics: dict[str, int] = {
            "offers_received": 0,
            "answers_published": 0,
            "error_answers_published": 0,
            "whep_errors": 0,
            "publish_errors": 0,
            "publish_acked": 0,
            "publish_nacked": 0,
        }
        # In-flight QoS 1 publishes awaiting broker ACK. Maps mid -> (topic, ts).
        self._pending_publishes: dict[int, tuple[str, float]] = {}
        # Last broker-side failure surfaced for diagnostics. Cloud status
        # can read this to populate the GCS dropdown's error tooltip.
        self._last_error: dict | None = None

    async def start(self, shutdown: asyncio.Event) -> None:
        """Connect to MQTT and relay SDP offers/answers until shutdown."""
        # Capture the running event loop so the paho on_message callback
        # (which fires on paho's worker thread) can schedule async work
        # back onto the main loop via run_coroutine_threadsafe.
        self._loop = asyncio.get_running_loop()

        # HTTP client for posting to local mediamtx WHEP endpoint.
        # Long-lived; reused across all signaling exchanges.
        self._http = httpx.AsyncClient(timeout=_WHEP_TIMEOUT_SEC)

        # Set up MQTT client
        client_id = f"ados-webrtc-{self._device_id}"
        self._mqtt = mqtt_client.Client(
            client_id=client_id,
            transport=self._transport,
            protocol=mqtt_client.MQTTv5,
        )

        # Signaling is bursty but tiny — bump inflight conservatively.
        self._mqtt.max_inflight_messages_set(100)

        if self._username:
            self._mqtt.username_pw_set(self._username, self._password)

        if self._transport == "websockets":
            self._mqtt.tls_set(cert_reqs=ssl.CERT_NONE)
            self._mqtt.tls_insecure_set(True)
            self._mqtt.ws_set_options(path="/mqtt")

        def on_connect(_client, _userdata, _flags, reason, _properties=None):
            # paho v5 callback signature: reason is a ReasonCode object
            # (or int for fallback). Anything non-zero means the broker
            # rejected the connection — record it so cloud status can
            # surface "broker_rejected" rather than spinning silently.
            rc = getattr(reason, "value", reason)
            if rc != 0:
                self._record_error("mqtt_connect_rejected", rc)
                log.error(
                    "webrtc_signaling_mqtt_connect_rejected",
                    broker=self._broker,
                    reason_code=rc,
                )
                return
            log.info("webrtc_signaling_mqtt_connected", broker=self._broker)
            # qos=1 so an offer arriving while we're in the middle of
            # processing a previous one is queued, not lost.
            self._mqtt.subscribe(self._topic_offer, qos=1)

        def on_disconnect(_client, _userdata, _flags, reason, _properties=None):
            rc = getattr(reason, "value", reason)
            if rc != 0:
                self._record_error("mqtt_disconnected", rc)
                log.warning(
                    "webrtc_signaling_mqtt_disconnected",
                    broker=self._broker,
                    reason_code=rc,
                )

        def on_publish(_client, _userdata, mid, _reason=None, _properties=None):
            """Broker ACKed a QoS 1 publish. Drop from pending."""
            self._pending_publishes.pop(mid, None)
            self._metrics["publish_acked"] += 1

        def on_message(_client, _userdata, msg):
            """Paho callback — runs on paho's worker thread, NOT the asyncio loop.

            We MUST schedule the actual handling onto the main loop via
            run_coroutine_threadsafe. Same gotcha that MavlinkMqttRelay
            already documents.
            """
            try:
                sdp_offer = msg.payload.decode("utf-8", errors="replace")
            except Exception as exc:
                log.warning("webrtc_signaling_decode_failed", error=str(exc))
                return
            self._metrics["offers_received"] += 1
            log.info("webrtc_signaling_offer_received", offer_size=len(sdp_offer))
            if self._loop is None:
                return
            # Fire-and-forget: schedule the async handler. The paho
            # callback returns immediately so the worker thread isn't
            # blocked on the WHEP round-trip.
            asyncio.run_coroutine_threadsafe(
                self._handle_offer(sdp_offer), self._loop
            )

        self._mqtt.on_message = on_message
        self._mqtt.on_connect = on_connect
        self._mqtt.on_disconnect = on_disconnect
        self._mqtt.on_publish = on_publish

        try:
            self._mqtt.connect(self._broker, self._port, keepalive=60)
        except Exception as e:
            log.error("webrtc_signaling_mqtt_connect_failed", error=str(e))
            await self._http.aclose()
            return

        self._mqtt.loop_start()
        log.info(
            "webrtc_signaling_started",
            device_id=self._device_id,
            topic_offer=self._topic_offer,
            topic_answer=self._topic_answer,
        )

        # Periodic metric logging + shutdown wait
        try:
            while not shutdown.is_set():
                await asyncio.sleep(_METRIC_LOG_INTERVAL)
                log.info("webrtc_signaling_metrics", **self._metrics)
        except asyncio.CancelledError:
            pass
        finally:
            await self.stop()

    @property
    def last_error(self) -> dict | None:
        """Most recent broker-side failure for diagnostics. None if healthy."""
        return self._last_error

    def _record_error(self, code: str, detail: int | str | None = None) -> None:
        """Stash the most recent broker-side failure for cloud status to surface."""
        self._last_error = {
            "code": code,
            "detail": detail,
            "ts": time.time(),
        }

    def _publish_with_tracking(self, payload: bytes, label: str) -> bool:
        """publish() wrapper that records broker rejections and tracks ACKs.

        paho's publish() returns immediately with an MQTTMessageInfo whose
        rc field signals queue-time failure (e.g., not connected). For QoS 1
        the actual broker ACK arrives later via on_publish; we track the
        mid so a future watchdog can flag stuck messages.
        """
        if not self._mqtt:
            return False
        try:
            info = self._mqtt.publish(self._topic_answer, payload, qos=1)
        except Exception as exc:
            self._metrics["publish_errors"] += 1
            self._record_error("publish_exception", str(exc))
            log.warning(
                "webrtc_signaling_publish_exception",
                label=label,
                error=str(exc),
            )
            return False
        rc = getattr(info, "rc", 0)
        if rc != 0:
            self._metrics["publish_errors"] += 1
            self._metrics["publish_nacked"] += 1
            self._record_error("publish_rejected", rc)
            log.warning(
                "webrtc_signaling_publish_rejected",
                label=label,
                rc=rc,
            )
            return False
        self._pending_publishes[info.mid] = (self._topic_answer, time.time())
        return True

    def _publish_error(self, error: str, status: int = 0) -> None:
        """Publish a JSON error to the answer topic so the browser fails fast.

        SDP always starts with ``v=0``; JSON always starts with ``{``.
        The browser distinguishes them with a single-character check.
        """
        if not self._mqtt:
            return
        payload = json.dumps({"error": error, "status": status}).encode("utf-8")
        if self._publish_with_tracking(payload, label="error"):
            self._metrics["error_answers_published"] += 1
            log.info(
                "webrtc_signaling_error_published",
                error=error,
                status=status,
            )

    async def _handle_offer(self, sdp_offer: str) -> None:
        """Forward an SDP offer to local mediamtx and publish the answer.

        Runs on the main asyncio loop (scheduled via run_coroutine_threadsafe
        from the paho on_message thread callback).
        """
        if self._http is None or self._mqtt is None:
            return
        try:
            resp = await self._http.post(
                _LOCAL_WHEP_URL,
                content=sdp_offer,
                headers={"Content-Type": "application/sdp"},
            )
            if resp.status_code >= 300:
                self._metrics["whep_errors"] += 1
                log.warning(
                    "webrtc_signaling_whep_failed",
                    status=resp.status_code,
                    body=resp.text[:200],
                )
                self._publish_error("whep_failed", resp.status_code)
                return
            sdp_answer = resp.text
        except Exception as exc:
            self._metrics["whep_errors"] += 1
            log.warning("webrtc_signaling_whep_exception", error=str(exc))
            self._publish_error("whep_exception", 0)
            return

        if self._publish_with_tracking(sdp_answer.encode("utf-8"), label="answer"):
            self._metrics["answers_published"] += 1
            log.info(
                "webrtc_signaling_answer_published",
                offer_size=len(sdp_offer),
                answer_size=len(sdp_answer),
            )

    async def stop(self) -> None:
        """Disconnect MQTT and HTTP client. Idempotent."""
        if self._mqtt:
            try:
                self._mqtt.loop_stop()
                self._mqtt.disconnect()
            except Exception:
                pass
            self._mqtt = None
        if self._http:
            try:
                await self._http.aclose()
            except Exception:
                pass
            self._http = None
        log.info("webrtc_signaling_stopped", **self._metrics)
