# Local Wireless Mesh (batman-adv)

The ADOS Ground Agent uses batman-adv to carry traffic between `relay` and `receiver` nodes when the deployment spans more than one physical site. This doc covers what batman-adv is, why it fits a ground-station deployment, what hardware it requires, and what to expect in the field.

## What batman-adv is

batman-adv is a mainline Linux kernel module that turns a group of WiFi devices into a self-organizing, self-healing Layer-2 mesh. Nodes advertise themselves using OGMv2 messages once per second and compute a Transmit Quality (TQ) metric against every neighbor. The module exposes a single virtual interface named `bat0` that the rest of the network stack sees as a normal Ethernet device, so IP, UDP, and mDNS work without modification.

There is no central coordinator. Every node in the mesh is equal. The kernel handles routing, dead-neighbor detection (~3-5 s after OGM loss), automatic rerouting, and partition reunification when two halves of the mesh can hear each other again.

## Why the Ground Agent uses it

The ground side of a WFB-ng deployment often has to forward fragments from field-placed relay nodes to a hub receiver. Options considered:

- **Wired Ethernet**. reliable, low latency, but field sites rarely have Ethernet.
- **Shared WiFi AP / router**. reliable but needs infrastructure that does not exist at most flying sites.
- **batman-adv on a second USB WiFi dongle**. self-organizing, zero infrastructure, graceful under partition.

batman-adv is the default because it works with nothing but the nodes themselves. If a deployment does have Ethernet or a shared AP, the mesh still runs and batman-adv will pick the lower-latency path automatically.

## Hardware required per node

| Role | Primary WiFi adapter (WFB-ng) | Secondary WiFi adapter (mesh carrier) |
|---|---|---|
| `direct` | 1x RTL8812EU | not needed |
| `relay` | 1x RTL8812EU | 1x generic USB WiFi adapter |
| `receiver` | 1x RTL8812EU | 1x generic USB WiFi adapter |

**Mesh carrier requirements:**

- Linux driver that supports 802.11s (preferred) or IBSS (fallback). Verify with `iw list`.
- USB 2.0 port is sufficient. USB 3.0 is fine.
- Either 2.4 GHz or 5 GHz will work. Avoid putting the mesh carrier on the same channel as the WFB-ng primary to prevent self-interference.

The installer always pulls `batctl`, `avahi-daemon`, `wpasupplicant` (and `wpasupplicant-mesh-sae` or `wpad-mesh-wolfssl` when apt has them) on a ground-station profile. profile_detect scans for a second USB WiFi adapter and writes `mesh_capable: true` into `/etc/ados/profile.conf` when one is present. No install-time flag is required.

## Carrier modes

batman-adv rides on a Layer-2 carrier, chosen by `ground_station.mesh.carrier`:

1. **802.11s with SAE** (`802.11s`, default). wpa_supplicant mesh mode, SAE keyed from the deployment PSK, management-frame protection required.
2. **IBSS with IBSS-RSN** (`ibss`). wpa_supplicant ad-hoc mode, WPA2-PSK over CCMP keyed from the same PSK. For adapters without 802.11s.

Both carriers are always authenticated. The passphrase is the hex encoding of `/etc/ados/mesh/psk.key` (first 63 characters), so every node paired to the same receiver derives the same one. The wpa_supplicant config lives at `/run/ados/mesh/wpa_supplicant-<iface>.conf` (mode 0600). If the secure join cannot be brought up and verified, the node does not join the mesh at all; there is no open fallback.

Once up, `ados-batman` checks the mesh every 3 s: the interface still exists, its wpa_supplicant is alive, the interface is joined to the mesh ID, and it is still a member of `bat0`. On any loss it re-runs the secure join and the `batctl if add` bind, on the same fixed cadence, for as long as it takes. You can inspect the carrier in use from the setup webapp, Mission Control Hardware tab, or mesh health REST endpoint. The `carrier` field is either `80211s` or `ibss`.

## Cloud gateway election

Every relay and receiver can advertise itself as a cloud gateway on the mesh using batman-adv's gateway machinery. The node with WiFi client, Ethernet, or 4G sets:

```
batctl gw_mode server 10000/2000
```

where `10000/2000` is the advertised down/up capacity in kbit/s. Nodes that want a cloud path set:

```
batctl gw_mode client
```

The client measures TQ to each advertised gateway and routes cloud-bound traffic through the best one. If the selected gateway dies, batman-adv reselects in 3-5 s. With `ground_station.cloud_uplink: auto` the mode follows `/run/ados/uplink-active`: the 3 s supervision pass switches a node to `server` when its uplink comes up and back to `client` (receiver) or `off` (relay) when it drops, issuing `batctl gw_mode` only when the decision changes. `force_on` and `force_off` pin it. The agent exposes this via:

- Mesh gateways status: list every advertised gateway and the one currently selected.
- GET `/api/v1/ground-station/mesh/gateways`. same, as JSON

## mDNS

The receiver publishes a service record `_ados-receiver._tcp` on `bat0` via `avahi-daemon`. Relays resolve the record during role transition and during the `Mesh > Join mesh` OLED flow. Relay lookup is restricted to the same `/24` as the mesh interface to keep stray records from other networks from leaking in.

## What to expect in the field

**Bringup.** After the installer finishes and role is set to `relay` or `receiver`, batman-adv converges on the mesh within 5-10 s.

**Throughput.** Typical mesh throughput per hop: 3-6 Mbit/s on 2.4 GHz, 8-20 Mbit/s on 5 GHz, depending on chipset and signal quality. WFB-ng video fragments flowing from a relay are a few hundred kbit/s per drone, so even one-hop 2.4 GHz meshes are comfortable.

**Range.** Coverage between mesh nodes depends on the secondary dongle's antenna and output power. Small USB dongles with internal antennas work indoors and within 50-100 m line-of-sight. For longer hops, use a dongle with an external SMA antenna.

**Self-healing.** Neighbor loss is detected in 3-5 s; routes reconverge automatically. If the mesh splits and rejoins, batman-adv merges routes without intervention.

**What does NOT survive a split:** the receiver's mDNS record only lives on its side of the mesh. A relay that loses sight of the receiver cannot find it on a different side until the mesh merges.

## Related docs

- [relay-mode-setup.md](relay-mode-setup.md). how to bring up a relay
- [receiver-mode-setup.md](receiver-mode-setup.md). how to bring up a receiver
- [pairing-protocol.md](pairing-protocol.md). UDP 5801 invite flow that runs on top of batman-adv
- [cli-reference-mesh.md](cli-reference-mesh.md). Ground-station role and mesh actions
