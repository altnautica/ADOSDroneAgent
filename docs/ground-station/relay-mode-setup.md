# Relay Mode Setup

A relay is a ground node that receives WFB-ng fragments from the drone and forwards them over the local batman-adv mesh to the deployment's receiver. Use a relay when the drone flies through obstructed terrain or a long corridor and one ground node cannot cover the full flight area.

This doc walks through setting up a fresh node as a relay.

## Prerequisites

- A flashed SBC with the usual ground-station hardware: one RTL8812EU USB adapter for WFB-ng RX, one OLED on I2C, and four GPIO buttons.
- **One additional RTL8812EU USB adapter** used as the mesh carrier (default). Any 802.11s-capable USB WiFi dongle also works, but same-chip is the default for inventory simplicity and matching TX power.
- The ground-station-profile fingerprint. This is auto-detected on first boot (OLED + 4 buttons + RTL8812EU + no flight controller).
- A deployed receiver node already running and within radio range of where this relay will live.

## Install

From a fresh image, run the standard installer:

```
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh \
  | sudo bash
```

On a ground-station profile the install always includes these mesh steps; no flag required:

1. `apt install batctl avahi-daemon wpasupplicant iw`
2. Best-effort `apt install` of `wpasupplicant-mesh-sae` or `wpad-mesh-wolfssl` (802.11s SAE backend). If neither is available, the mesh carrier falls back to IBSS.
3. profile_detect fingerprint scans for a second USB WiFi adapter and sets `mesh_capable: true` in `/etc/ados/profile.conf` when present.
4. Creates `/etc/ados/mesh/` with 0755 permissions.

Reboot is not required. The mesh services stay masked until the role sentinel is set to `relay` or `receiver`.

## Role transition

Two ways to switch the node into `relay` role.

### From the OLED

1. Open the OLED menu.
2. Navigate to `Mesh > Set role`.
3. Pick `Relay`.
4. The node writes `/etc/ados/mesh/role` and begins transition.

The OLED shows a brief transitioning indicator. Within 5-15 s the `ados-batman.service` starts and `bat0` is up.

### From the CLI

```
sudo ados gs role set relay
```

Returns `transition queued`. Same effect as the OLED path.

The role transition fails with a `409` conflict if the relay has never been paired with a receiver. The mesh services need the mesh id and PSK that arrive in the invite bundle; without those files, `ados-batman.service` refuses to start.

## Join a receiver

On the relay's OLED:

1. Navigate to `Mesh > Join mesh`.
2. The screen scans for `_ados-receiver._tcp` mDNS records on `bat0`.
3. Pick the receiver from the scan list. The agent calls `POST /api/v1/ground-station/pair/join` internally.

The relay sends its X25519 public key in a join request to the receiver on UDP 5801. The receiver must currently have an Accept window open (see `receiver-mode-setup.md`).

The relay waits. The screen shows `Waiting for approval...`.

## Approval lands

When the operator at the receiver approves the pending relay, the receiver sends an encrypted invite bundle back to the relay on UDP 5801. The relay:

1. Decrypts the bundle with its X25519 private key + HKDF-derived ChaCha20-Poly1305 key.
2. Writes `/etc/ados/mesh/id`, `/etc/ados/mesh/psk.key` (0600), and the drone-paired WFB-ng RX key.
3. Triggers a restart of `ados-batman.service` and `ados-wfb-relay.service`.

Within about 10 s the OLED status flips to `Mesh: linked`. Video fragments start flowing from the relay to the receiver.

## What the relay runs

Under `relay` role, the supervisor starts three role-specific services. All three are stopped when role returns to `direct`.

| Service | Purpose | Command |
|---|---|---|
| `ados-batman.service` | batman-adv carrier bringup, neighbor + gateway monitoring, mesh event bus | `python -m ados.services.ground_station.mesh_manager` |
| `ados-wfb-relay.service` | Runs `wfb_rx -f <receiver_ip>` to decode local WFB-ng and forward surviving fragments to the receiver | `python -m ados.services.ground_station.wfb_relay` |
| mediamtx | Does NOT run on a relay. Only the receiver publishes a video stream. | |

You can confirm state with:

```
systemctl status ados-batman ados-wfb-relay
journalctl -u ados-wfb-relay -n 50 --no-pager
```

## Verification

On the relay:

```
ados gs role show            # role: relay, mesh_capable: true
ados gs mesh health          # up: true, carrier: 80211s (or ibss)
ados gs mesh neighbors       # at least the receiver listed, TQ > 100
```

On the receiver:

```
ados gs mesh neighbors       # relay MAC appears with reasonable TQ
ados gs wfb/receiver/relays  # confirms fragments are arriving
```

Open Mission Control on a device connected to the receiver's WiFi AP. The `Hardware > Distributed RX` sub-view shows the relay in the relay list with live packet-rate stats.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Role transition returns `409` | Relay has never been paired | Pair with a receiver first |
| OLED `Mesh > Join mesh` finds nothing | Mesh carrier not up, receiver not advertising, or different carrier | Check `ados gs mesh health` reports `up: true`; confirm the receiver is in range and running |
| Join request times out | Receiver has no open Accept window | Open an Accept window on the receiver and retry the Join |
| `ados-wfb-relay.service` fails to start | Mesh id or PSK missing on disk | Re-pair with the receiver; the bundle writes the missing files |
| Receiver sees relay in neighbors but no fragments arrive | Relay is forwarding to wrong IP | `systemctl restart ados-wfb-relay` so it re-reads the invite bundle |
| Neighbor TQ drops below 100 | Weak radio signal between the two mesh dongles | Reposition the relay, raise the antenna, or add a better external antenna |

## Revert to direct

If the relay should go back to standalone receive:

```
sudo ados gs role set direct
```

The mesh services stop, the WFB-ng receive path reverts to a single-node `wfb_rx` with local mediamtx, and the relay stops forwarding. Pairing state on disk is preserved so you can re-promote to `relay` later without repairing.

## Related docs

- [mesh-networking.md](mesh-networking.md). batman-adv background
- [receiver-mode-setup.md](receiver-mode-setup.md). the other side of the pair
- [pairing-protocol.md](pairing-protocol.md). what actually travels over UDP 5801
- [cli-reference-mesh.md](cli-reference-mesh.md). every `ados gs mesh` command
