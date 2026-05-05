# MQTT topic schema — cloud relay

Documents the MQTT topic surface every backend in this repository must
speak when publishing to the cloud relay broker. Source of truth: the
Python full agent under `src/ados/services/cloud/` and
`src/ados/services/mqtt/`.

## Naming convention

All topics are device-scoped under `ados/{device_id}/...` where
`device_id` is the persistent identifier set at first pair (read from
`config.agent.device_id`).

## Connection

| Setting | Value | Notes |
|---|---|---|
| Protocol | MQTT v5 | clients fall back to v3.1.1 if broker rejects v5 |
| Transport | TCP or WebSocket | per `config.server.mqtt_transport` |
| WebSocket path | `/mqtt` | when transport is WebSocket |
| Default broker port (TLS) | 8883 | TLS required for cloud mode |
| TLS | required in cloud mode | configured via `config.security.tls.*` |
| Client ID | `ados-{device_id}` | one connection per device |
| Username | `ados-{device_id}` | cloud mode |
| Password | API key from pairing | rotate via repair flow |
| Keep-alive | 60 s | |
| Last will / testament | not used | |

## Published topics — agent → cloud

### `ados/{device_id}/telemetry`

Vehicle state snapshot.

| Field | Value |
|---|---|
| QoS | 0 |
| Retained | false |
| Payload | JSON object (see "Telemetry payload" below) |
| Rate | configurable via `config.server.telemetry_rate` (default 2 Hz) |
| Backpressure | none — fire-and-forget |

### `ados/{device_id}/status`

Agent status snapshot.

| Field | Value |
|---|---|
| QoS | 1 |
| Retained | false |
| Payload | JSON: `{device_id, name, tier, armed, fc_connected}` |
| Rate | matches telemetry rate |

### `ados/{device_id}/mavlink/tx`

Raw MAVLink frames flowing FC → cloud → ground control station.

| Field | Value |
|---|---|
| QoS | 0 |
| Retained | false |
| Payload | raw MAVLink v2 binary, one frame per message |
| Rate | ~30 msg/s (FC frame rate) |
| Queue | bounded at 2000 frames; drop-oldest when full |
| Max in-flight | 1000 |

### `ados/{device_id}/webrtc/answer`

WebRTC SDP answer in response to inbound offers.

| Field | Value |
|---|---|
| QoS | 1 |
| Retained | false |
| Payload | SDP answer text |
| Rate | bursty (one per WebRTC session initiation) |
| Max in-flight | 100 |

## Subscribed topics — cloud → agent

### `ados/{device_id}/mavlink/rx`

GCS → FC commands routed back to the flight controller via the local
MAVLink IPC socket.

| Field | Value |
|---|---|
| QoS | 0 |
| Payload | raw MAVLink v2 binary forwarded to `/run/ados/mavlink.sock` |

### `ados/{device_id}/command`

Cloud-issued commands (other than MAVLink).

| Field | Value |
|---|---|
| QoS | not specified by broker; agent treats as at-most-once |
| Payload | JSON command object |

### `ados/{device_id}/webrtc/offer`

Browser-originated WebRTC SDP offers from a remote ground control
station.

| Field | Value |
|---|---|
| QoS | 1 |
| Payload | SDP offer text |

## Telemetry payload

JSON object emitted on `telemetry`:

```json
{
  "mav_type": 2,
  "autopilot": 3,
  "armed": true,
  "mode": "GUIDED",
  "position": {
    "lat": 37.7749,
    "lon": -122.4194,
    "alt_msl": 30.5,
    "alt_rel": 5.0,
    "heading": 270.0
  },
  "velocity": {
    "vx": 0.5,
    "vy": -0.1,
    "vz": 0.0,
    "groundspeed": 0.5,
    "airspeed": 0.0,
    "climb": 0.0
  },
  "attitude": {
    "roll": 0.0,
    "pitch": 0.0,
    "yaw": 4.71
  },
  "battery": {
    "voltage": 22.4,
    "current": 5.2,
    "remaining": 78,
    "temperature": null,
    "cell_voltages": []
  },
  "gps": {
    "fix_type": 3,
    "satellites": 12,
    "eph": 0.8,
    "epv": 1.2
  },
  "rc": {
    "channels": [1500, 1500, 1500, 1500, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    "rssi": 95
  },
  "throttle": 50,
  "last_heartbeat": "2026-05-05T18:30:12.345Z",
  "last_update": "2026-05-05T18:30:12.500Z"
}
```

Field origins are defined in `src/ados/services/mavlink/state.py`
(VehicleState dataclass). Backends serializing state for this topic
must produce field names byte-for-byte equivalent.

## Conformance

A backend implementation is conformant when it:

1. Publishes to all four published topics with the documented QoS, rate, and payload format.
2. Subscribes to all three subscribed topics with the documented QoS.
3. Honors the device-scoped naming convention `ados/{device_id}/...` for every topic.
4. Speaks MQTT v5 (with v3.1.1 fallback) over TLS in cloud mode.
5. Reuses `client_id = "ados-{device_id}"` so the broker can enforce one-active-connection-per-device semantics.

Backends must NOT invent new top-level scopes or rename existing topic strings.
