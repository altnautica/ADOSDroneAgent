# REST API Reference: Role, Mesh, Pairing

Routes under `/api/v1/ground-station/*` related to deployment role, batman-adv local wireless mesh, and field-only OLED pairing. Every route requires the `X-ADOS-Key` header set to the node's `ADOS_API_KEY`. Every route returns JSON unless otherwise noted. All routes are gated on the node running in `ground-station` profile; on a drone-profile node these routes return `404`.

## Role

### GET /role

Current deployment role and capability state.

```
GET /api/v1/ground-station/role
200 OK
{
  "role": "direct",
  "mesh_capable": true,
  "pending": null
}
```

Fields:

- `role`. one of `direct`, `relay`, `receiver`
- `mesh_capable`. `true` if the node has batctl and a second WiFi dongle
- `pending`. the role the node is transitioning to, or `null`

### PUT /role

Set the deployment role. Body: `{"role": "<direct|relay|receiver>"}`.

```
PUT /api/v1/ground-station/role
{"role": "receiver"}

202 Accepted
{"transitioning_to": "receiver"}
```

Errors:

- `400` invalid role value
- `403` mesh role requested but `mesh_capable` is `false`
- `409` relay role requested without a paired invite bundle on disk

The agent writes `/etc/ados/mesh/role` and restarts service units gated on that sentinel.

## Mesh

### GET /mesh

Top-level batman-adv state snapshot.

```
GET /api/v1/ground-station/mesh
200 OK
{
  "role": "receiver",
  "up": true,
  "mesh_iface": "wlan1",
  "bat_iface": "bat0",
  "carrier": "80211s",
  "mesh_id": "ados-3f2a7b91c8",
  "selected_gateway": "02:aa:bb:cc:dd:ee",
  "partition": false,
  "started_at_ms": 1713339120000,
  "last_poll_ms": 1713339246010
}
```

### GET /mesh/neighbors

Every directly visible batman-adv neighbor.

```
GET /api/v1/ground-station/mesh/neighbors
200 OK
{
  "neighbors": [
    {"mac": "02:aa:bb:cc:dd:ee", "iface": "bat0", "tq": 242, "last_seen_ms": 150},
    {"mac": "02:11:22:33:44:55", "iface": "bat0", "tq": 196, "last_seen_ms": 820}
  ]
}
```

### GET /mesh/routes

Per-destination next-hop resolution.

```
GET /api/v1/ground-station/mesh/routes
200 OK
{
  "routes": [
    {"dest": "02:11:22:33:44:55", "next_hop": "02:aa:bb:cc:dd:ee", "hops": 2}
  ]
}
```

### GET /mesh/gateways

Every advertised gateway on the mesh.

```
GET /api/v1/ground-station/mesh/gateways
200 OK
{
  "gateways": [
    {"mac": "02:aa:bb:cc:dd:ee", "class_up_kbps": 2000, "class_down_kbps": 10000, "tq": 242, "selected": true},
    {"mac": "02:11:22:33:44:55", "class_up_kbps": 1000, "class_down_kbps": 5000, "tq": 196, "selected": false}
  ]
}
```

### PUT /mesh/gateway_preference

Nudge batman-adv to prefer a specific gateway MAC. A setting of `null` returns to automatic selection.

```
PUT /api/v1/ground-station/mesh/gateway_preference
{"mac": "02:aa:bb:cc:dd:ee"}

200 OK
{"applied": "02:aa:bb:cc:dd:ee"}
```

### GET /mesh/config

Read the mesh carrier configuration.

```
GET /api/v1/ground-station/mesh/config
200 OK
{
  "mesh_id": "ados-3f2a7b91c8",
  "channel": 6,
  "carrier_mode": "80211s",
  "iface_hint": null
}
```

### PUT /mesh/config

Change mesh config. Takes effect on next `ados-batman.service` restart.

```
PUT /api/v1/ground-station/mesh/config
{"channel": 11}

200 OK
{"channel": 11}
```

## WFB-ng role observability

### GET /wfb/relay/status

Relay only. Live forwarder status: target receiver IP, packets forwarded, last keepalive.

### GET /wfb/receiver/relays

Receiver only. Every relay that has delivered at least one fragment in the last 30 s, with its packet rate.

### GET /wfb/receiver/combined

Receiver only. Post-FEC-combine output stream stats: kbit/s, fraction of packets recovered from relay contributions, fraction recovered from local monitor.

## Pairing (receiver-hosted UDP 5801)

### POST /pair/accept

Receiver only. Open a pairing accept window. Body: `{"window_s": 60}`.

```
POST /api/v1/ground-station/pair/accept
{"window_s": 60}

200 OK
{
  "open": true,
  "opened_at_ms": 1713339123000,
  "closes_at_ms": 1713339183000
}
```

### POST /pair/close

Receiver only. Close the accept window immediately.

### GET /pair/pending

Receiver only. Pending invite requests that have not yet been approved.

```
GET /api/v1/ground-station/pair/pending
200 OK
{
  "window": {
    "open": true,
    "opened_at_ms": 1713339123000,
    "closes_at_ms": 1713339183000
  },
  "pending": [
    {"device_id": "ados-1a2b3c", "remote_ip": "10.20.0.14", "received_at_ms": 1713339130000}
  ]
}
```

### POST /pair/approve/{device_id}

Receiver only. Admit the specified pending relay. The receiver derives a session key via X25519 ECDH against the relay's pubkey, builds the invite bundle (mesh id, PSK, drone WFB-ng rx key, receiver mDNS host and port, TTL), ChaCha20-Poly1305-seals it, and sends it to the relay on UDP 5801. The transmission is sent twice with a 100 ms gap to survive a single dropped packet.

```
POST /api/v1/ground-station/pair/approve/ados-1a2b3c
200 OK
{"approved_at_ms": 1713339140000}
```

### POST /pair/revoke/{device_id}

Receiver only. Add the device to `/etc/ados/mesh/revocations.json`. Future invite requests from that device id are rejected until it is removed from the revocation list.

### POST /pair/join

Relay only. Send an invite request to a receiver. Body: `{"host": "<ip>", "port": 5801}`. With an empty body, the relay resolves `_ados-receiver._tcp` over mDNS on `bat0` and picks the first result.

```
POST /api/v1/ground-station/pair/join
{"host": "10.20.0.1", "port": 5801}

200 OK
{"sent_at_ms": 1713339131000}
```

## Error codes

| Code | Body | Meaning |
|---|---|---|
| `400` | `invalid role` / `invalid window_s` / `device_id not hex` | Request payload failed validation |
| `401` | empty | `X-ADOS-Key` missing or wrong |
| `403` | `ground_station profile only` / `mesh_capable=false` / `role mismatch` | Profile or role gate blocks the route |
| `404` | empty | Route only exists in ground-station profile, or pending device id not found |
| `409` | `relay requires invite bundle` / `accept window already open` | State conflict |
| `410` | empty | Pairing window has closed since the client last polled |
| `500` | `<message>` | Internal error (service crashed, batctl missing, etc.) |
| `503` | `pairing_bind_failed` | UDP 5801 could not be bound on `bat0`; check that batman is up |

## WebSocket streams

The agent also exposes a mesh event stream.

### WS /ws/mesh

Subscribe to mesh state changes. Each message is one JSON object from the `MeshEventBus`.

```
kind="neighbor_seen"    {"mac": "...", "tq": 242}
kind="neighbor_lost"    {"mac": "..."}
kind="gateway_changed"  {"old_mac": "...", "new_mac": "..."}
kind="partition_heal"   {"merged_with": "<mesh_id>"}
kind="role_changed"     {"from": "direct", "to": "receiver"}
kind="pair_pending"     {"device_id": "ados-1a2b3c"}
kind="pair_approved"    {"device_id": "ados-1a2b3c"}
```
