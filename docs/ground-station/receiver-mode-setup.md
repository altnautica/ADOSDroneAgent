# Receiver Mode Setup

A receiver is the hub of a distributed-receive deployment. It combines WFB-ng fragments heard on its own radio with fragments forwarded by every paired relay, runs Reed-Solomon FEC combine across the merged stream, and publishes the clean video on the same downstream pipeline a single-node `direct` setup uses (mediamtx, WiFi AP, HDMI kiosk, browser WebRTC).

This doc walks through setting up a fresh node as a receiver and admitting relays into its mesh.

## Prerequisites

- A flashed SBC with the usual ground-station hardware: one RTL8812EU USB adapter for WFB-ng RX, one OLED on I2C, and four GPIO buttons.
- **One additional RTL8812EU USB adapter** used as the mesh carrier (default). Any 802.11s-capable USB WiFi dongle also works, but same-chip is the default for inventory simplicity and matching TX power.
- The ground-station-profile fingerprint (auto-detected at boot).
- The drone paired and transmitting on a known channel.

One node in a distributed-receive deployment becomes the receiver; the rest are relays.

## Install

Run the standard installer:

```
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh \
  | sudo bash
```

On a ground-station profile, this install always pulls `batctl`, `avahi-daemon`, `wpasupplicant`, and a best-effort 802.11s SAE backend. profile_detect fingerprints the second USB WiFi adapter and writes `mesh_capable: true` to `/etc/ados/profile.conf`. Role stays at `direct` until it is explicitly set.

## Role transition

Two paths.

### From the OLED

1. Open the OLED menu.
2. Navigate to `Mesh > Set role`.
3. Pick `Receiver`.

The node writes `/etc/ados/mesh/role = receiver`. On this first transition the agent also generates a new mesh identity:

- `/etc/ados/mesh/id`. 16-char stable identifier derived from the device id (`ados-<10 hex chars>`).
- `/etc/ados/mesh/psk.key`. 32-byte shared PSK, mode 0600.
- `/etc/ados/mesh/keypair.json`. X25519 key pair used for invite bundle signing.

Within 5-15 s `ados-batman.service` and `ados-wfb-receiver.service` are up. `bat0` carries the mesh and `avahi-daemon` publishes the `_ados-receiver._tcp` record on it.

### From the CLI

```
sudo ados gs role set receiver
```

Same effect. Returns `transition queued`.

## Open an Accept window

Before any relay can join, the receiver has to open an Accept window.

### From the OLED

1. Navigate to `Mesh > Accept relay`.
2. Press Select. A countdown begins (defaults to 60 s).
3. As relays send join requests, they appear inline on the same screen with an Approve action.

### From the CLI

```
ados gs mesh accept 60
```

or equivalently:

```
curl -X POST -H "X-ADOS-Key: $ADOS_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"window_s": 60}' \
  http://localhost:8080/api/v1/ground-station/pair/accept
```

## Admit relays

When a relay sends a join request during an open window, the receiver appends it to the pending list. From the OLED:

1. Scroll through pending entries on the `Accept relay` screen.
2. Press Select on each device id to approve.
3. The receiver derives a ChaCha20-Poly1305 session key via X25519 ECDH against the relay's pubkey, builds the invite bundle (mesh id, PSK, drone channel, WFB-ng RX key, mDNS host, mDNS port), seals it, and sends it to the relay on UDP 5801 (twice, 100 ms apart).

From the CLI:

```
ados gs mesh pending          # list pending device ids
ados gs mesh approve <id>     # admit one
ados gs mesh revoke <id>      # reject or remove an approved relay
```

Closing the Accept window:

```
curl -X POST -H "X-ADOS-Key: $ADOS_API_KEY" \
  http://localhost:8080/api/v1/ground-station/pair/close
```

or wait for the countdown to expire.

## What the receiver runs

| Service | Purpose | Command |
|---|---|---|
| `ados-batman.service` | batman-adv carrier bringup, neighbor and gateway monitoring, mesh event bus, mDNS publisher | `python -m ados.services.ground_station.mesh_manager` |
| `ados-wfb-receiver.service` | Runs `wfb_rx -a` to aggregate fragments from the local monitor adapter plus every forwarder, runs FEC combine across the merged stream, emits the clean UDP stream that mediamtx consumes | `python -m ados.services.ground_station.wfb_receiver` |
| `ados-mediamtx-gs.service` | RTSP ingest from the receiver, WebRTC + HLS output for browser viewers | |
| `ados-hostapd.service` | WiFi AP so browsers can connect | |

Confirm with:

```
systemctl status ados-batman ados-wfb-receiver ados-mediamtx-gs
journalctl -u ados-wfb-receiver -n 50 --no-pager
```

## FEC combine

With `wfb_rx -a`, the receiver runs Reed-Solomon FEC (k=8, n=12 by default) across the union of fragments it heard locally plus every fragment forwarded by each approved relay. If one node hears packets {1,3,5,7} and another hears {2,4,6,8}, the merged stream has all eight and decodes cleanly. Each relay does NOT have to cover the full stream on its own; the receiver composes coverage from every contributor.

## Cloud uplink and gateway election

Any node on the mesh (receiver OR any relay) with an upstream internet path (WiFi client, Ethernet, or 4G) can advertise itself as a batman-adv gateway. The receiver's mesh service measures TQ to each advertised gateway and routes cloud-bound traffic through the best one.

Inspect with:

```
ados gs mesh gateways
```

Nudge a specific gateway with:

```
curl -X PUT -H "X-ADOS-Key: $ADOS_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"mac": "<gateway-mac>"}' \
  http://localhost:8080/api/v1/ground-station/mesh/gateway_preference
```

Set `"mac": null` to return to automatic selection.

## Verification

On the receiver:

```
ados gs role show                      # role: receiver
ados gs mesh health                    # up: true
ados gs mesh neighbors                 # every approved relay listed
curl -s -H "X-ADOS-Key: $ADOS_API_KEY" \
  http://localhost:8080/api/v1/ground-station/wfb/receiver/relays | jq
curl -s -H "X-ADOS-Key: $ADOS_API_KEY" \
  http://localhost:8080/api/v1/ground-station/wfb/receiver/combined | jq
```

In Mission Control (on a device connected to the receiver's WiFi AP), open the `Hardware > Distributed RX` sub-view. It lists every relay with live packet rates and shows combined-stream stats.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `ados-batman.service` fails with `mesh_id missing` | First-boot receiver never reached the code path that generates id + PSK | `sudo rm /etc/ados/mesh/role && sudo ados gs role set receiver` to retrigger the init code |
| `Accept relay` screen shows no pending | Relays never sent a join request during the window | Confirm relay finished role transition; open a new Accept window; confirm mesh is up on both sides |
| Approved relay never transitions to linked | Invite bundle was lost on both retransmits | Re-approve the relay or ask the relay to press Join mesh again |
| FEC combined output has high packet loss | Relay fragment stream is lagging (network congestion on the mesh) | Confirm mesh TQ > 150 to each relay; move the relay closer; switch mesh channel to a less congested one |
| Receiver published no `_ados-receiver._tcp` record | `avahi-daemon` not running | `systemctl status avahi-daemon`; restart if stopped |

## Revert to direct

```
sudo ados gs role set direct
```

The receiver services stop, the node reverts to single-node WFB-ng RX via local `wfb_rx` and mediamtx. Mesh identity files stay on disk so you can re-promote to receiver later without regenerating the mesh.

## Related docs

- [mesh-networking.md](mesh-networking.md). batman-adv background
- [relay-mode-setup.md](relay-mode-setup.md). the other side of the pair
- [pairing-protocol.md](pairing-protocol.md). UDP 5801 invite flow internals
- [cli-reference-mesh.md](cli-reference-mesh.md). every `ados gs mesh` command
