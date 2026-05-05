# State IPC wire format

Documents the wire format the agent publishes on the state IPC socket at `/run/ados/state.sock`. Source of truth: `StateIPCServer` and `StateIPCClient` in `src/ados/core/ipc.py`.

## Socket

| Field | Value |
|---|---|
| Path | `/run/ados/state.sock` |
| Type | Unix-domain stream socket |
| Permissions | `0o666` (any local user can connect) |
| Auth | none — local-only by socket file permissions |

## Framing

Newline-delimited JSON. Each message is one complete JSON object terminated by `\n`. There is no length prefix and no message envelope; the JSON object itself is the message.

```
{"key":"value",...}\n
{"key":"value",...}\n
```

Reader pseudocode:

```python
async for raw_line in reader:                  # tokio: read_line()
    if not raw_line:
        break                                   # EOF
    try:
        msg = json.loads(raw_line)
    except json.JSONDecodeError:
        continue                                # silently skip malformed lines
    handle(msg)
```

Writer pseudocode:

```python
writer.write(json.dumps(state).encode() + b"\n")
await writer.drain()
```

## Cadence and delivery

| Field | Value |
|---|---|
| Publish rate | 10 Hz |
| Delivery | best-effort broadcast to all subscribed clients |
| Per-client queue | bounded at 32 snapshots (~3 s of buffered state) |
| Slow-client policy | client whose queue fills is disconnected; must reconnect |
| Initial snapshot | last known state delivered immediately on connect |

## Message shape — vehicle state snapshot

The snapshot is a JSON object derived from the `VehicleState` dataclass in `src/ados/services/mavlink/state.py`. Field names are stable; backends serializing state for this socket must produce these exact names.

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

Field semantics:

- `mav_type`: MAV_TYPE enum value (e.g. 2 = MAV_TYPE_QUADROTOR).
- `autopilot`: MAV_AUTOPILOT enum value (e.g. 3 = MAV_AUTOPILOT_ARDUPILOTMEGA).
- `armed`: boolean armed/disarmed state.
- `mode`: ASCII flight-mode string as reported by the FC.
- `position.alt_msl`: altitude above mean sea level in meters.
- `position.alt_rel`: altitude relative to home in meters.
- `position.heading`: heading in degrees, 0-360.
- `velocity.{vx,vy,vz}`: m/s in the NED body-fixed frame.
- `velocity.groundspeed` / `airspeed` / `climb`: m/s.
- `attitude.{roll,pitch,yaw}`: radians, ENU convention.
- `battery.remaining`: percent 0-100 or `null` when not reported.
- `battery.cell_voltages`: list of per-cell volts; empty when not reported.
- `gps.fix_type`: 0=no fix, 1=2D, 2=3D, 3=DGPS, 4=RTK float, 5=RTK fixed.
- `rc.channels`: 18 channels of normalized PWM (typically 1000-2000); 0 when channel is not present.
- `last_heartbeat` / `last_update`: ISO 8601 UTC timestamps.

## No aggregated state file

There is intentionally no aggregated `/run/ados/state.json` file on disk. Consumers connect to the socket and process the live stream. This avoids stale-snapshot races and keeps the state topology to a single canonical source.

## Conformance

A backend implementing the state IPC server must:

1. Bind a Unix-domain stream socket at `/run/ados/state.sock` with mode `0o666`.
2. Accept arbitrarily many concurrent client connections.
3. On accept, send the most recent snapshot immediately (or wait for the next tick if no snapshot exists yet).
4. Publish at 10 Hz when the FC is connected and once per heartbeat-interval otherwise.
5. Disconnect clients whose receive queue fills (default threshold 32 messages).
6. Emit each snapshot as a single line of UTF-8 JSON terminated by `\n`.
7. Use the exact field names documented above so downstream consumers (Mission Control, plugins, future backends) read the same shape from any agent implementation.
