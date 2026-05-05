# proto/cloud/ — cloud relay contract

Documents the wire surface every backend speaks to the cloud relay broker and to remote ground control stations.

## Files

| File | Purpose | Status |
|---|---|---|
| `mqtt-topics.md` | MQTT topic schema for telemetry, commands, video signaling | TODO |
| `openapi.yaml` | HTTP heartbeat + pairing API surface | TODO |
| `rtsp-conventions.md` | RTSP path patterns for cloud-relay video push | TODO |

## Source of truth

The Python full agent under `src/ados/services/cloud/` is the reference implementation. Contract documents in this directory describe the wire surface it speaks today; alternate backends conform to the same surface byte-for-byte.

## Topic naming convention

All MQTT topics are device-scoped under `ados/{device_id}/...` with the device identifier set at first pair. Topic strings are stable; QoS levels are stable. Backends must not invent new top-level scopes.
