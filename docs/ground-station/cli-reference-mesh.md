# CLI Reference: Role and Mesh

Full reference for the `ados gs role` and `ados gs mesh` command groups. Both groups only work on a ground-station-profile node. On a drone-profile node the commands return a profile error from the API. Every call talks to the local REST API at `http://localhost:8080/api/v1/ground-station/*` and requires the node's `ADOS_API_KEY` (read from `/etc/ados/env`).

## ados gs role

### ados gs role show

Print the current deployment role, whether mesh is capable on this hardware, and whether a pending role transition is in flight.

```
$ ados gs role show
role:          direct
mesh_capable:  true
pending:       none
```

Fields:

| Field | Meaning |
|---|---|
| `role` | One of `direct`, `relay`, `receiver` |
| `mesh_capable` | `true` when the node has a second WiFi dongle and batctl + avahi-daemon installed |
| `pending` | `none` or the role the node is transitioning to |

### ados gs role set `<role>`

Switch the node into a different deployment role. Valid values: `direct`, `relay`, `receiver`.

```
$ ados gs role set receiver
transition queued. services restarting in the background.
```

The agent writes `/etc/ados/mesh/role`, stops the services that no longer apply, and starts the ones the new role needs. Expect a short period (5-15 s) where the node is visible in `ados gs status` as transitioning.

Failure modes:

- `403 role transition requires mesh capability`. plug a second USB WiFi adapter into this node; profile_detect will fingerprint it on the next boot (or after `systemctl restart ados-bootstrap` on the running node) and flip `mesh_capable: true` in `/etc/ados/profile.conf`.
- `409 relay requires an approved invite bundle`. a relay needs to join a receiver's Accept window before the role can switch. See `relay-mode-setup.md`.

## ados gs mesh

### ados gs mesh health

Top-level mesh state. Prints one block of key-value pairs.

```
$ ados gs mesh health
carrier:           80211s
mesh_iface:        wlan1
bat_iface:         bat0
mesh_id:           ados-3f2a7b91c8
up:                true
partition:         false
selected_gateway:  02:aa:bb:cc:dd:ee
started_at_ms:     1713339120000
```

### ados gs mesh neighbors

Every directly-visible batman-adv neighbor with its transmit-quality score and last-seen age.

```
$ ados gs mesh neighbors
MAC                 IFACE   TQ   LAST_SEEN_MS
02:aa:bb:cc:dd:ee   bat0    242  150
02:11:22:33:44:55   bat0    196  820
```

TQ is a batman-adv-scale value from 0 (unreachable) to 255 (direct neighbor, no loss).

### ados gs mesh gateways

Every advertised gateway on the mesh, the advertised up/down capacity, the TQ to reach it, and whether batman-adv is currently routing through it.

```
$ ados gs mesh gateways
MAC                 UP_KBPS  DOWN_KBPS  TQ   SELECTED
02:aa:bb:cc:dd:ee   2000     10000      242  yes
02:11:22:33:44:55   1000     5000       196  no
```

### ados gs mesh route `<dest-mac>`

Resolve the next-hop path to a specific MAC on the mesh.

```
$ ados gs mesh route 02:11:22:33:44:55
dest:     02:11:22:33:44:55
next_hop: 02:aa:bb:cc:dd:ee
hops:     2
```

### ados gs mesh accept `<window-s>`

Receiver only. Open a pairing accept window for the given number of seconds (defaults to 60). During the window, any relay that sends an invite request to the node's `bat0` on UDP 5801 appears in `ados gs mesh pending`.

```
$ ados gs mesh accept 60
accept window open. closes in 60 s.
```

### ados gs mesh pending

Receiver only. List relays that have sent an invite request but are not yet approved.

```
$ ados gs mesh pending
DEVICE_ID     REMOTE_IP       RECEIVED_AT_MS
ados-1a2b3c   10.20.0.14      1713339123123
ados-4d5e6f   10.20.0.15      1713339125450
```

### ados gs mesh approve `<device-id>`

Receiver only. Admit a pending relay into the mesh. The receiver packs the mesh id, PSK, drone-paired WFB-ng rx key, and mDNS host and port into a signed encrypted invite bundle and sends it to the relay on UDP 5801. The relay writes the bundle into `/etc/ados/mesh/` and restarts its mesh and wfb-relay services.

```
$ ados gs mesh approve ados-1a2b3c
approved. invite bundle delivered (2 send attempts).
```

### ados gs mesh revoke `<device-id>`

Receiver only. Remove a previously approved relay. The device id is added to `/etc/ados/mesh/revocations.json` and any future invite request from that device is rejected until it is removed from the revocation list.

```
$ ados gs mesh revoke ados-1a2b3c
revoked. device added to revocations.
```

### ados gs mesh join `[--host <ip>] [--port <port>]`

Relay only. Sends a join request to a receiver. If `--host` and `--port` are omitted, the relay resolves `_ados-receiver._tcp` over mDNS on `bat0` and picks the first result. This command is usually driven from the OLED (`Mesh > Join mesh`), not the shell.

```
$ ados gs mesh join --host 10.20.0.1 --port 5801
join request sent. receiver must approve.
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `ados gs mesh health` shows `up: false` | Mesh dongle not detected or driver not loaded | `dmesg \| grep -i usb`, `iw list`, confirm a second WiFi adapter is present |
| `ados gs mesh health` reports `carrier: none` | 802.11s SAE not available; IBSS fallback failed | `sudo apt install wpasupplicant-mesh-sae` or `wpad-mesh-wolfssl`; fallback to IBSS is automatic otherwise |
| `ados gs mesh pending` stays empty after relay joined | Relay never reached the receiver on UDP 5801 | Confirm both nodes are on the same mesh carrier; check `bat0` is up on the receiver |
| `ados gs mesh approve` returns a pubkey mismatch | Stale pending entry after relay rebooted | Ask the relay to resend (`Mesh > Join mesh` again), approve the new entry |
| Gateway column shows `selected: no` for every row | Local uplink is preferred or no gateway is reachable | `ados gs mesh gateways` + `ados gs network show` to see which uplink won |
