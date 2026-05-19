# Inter-rig transport layers

This page describes how the drone and the ground station talk to each other,
which transport carries which kind of traffic, and the rules that govern when
each one is used. ADOS Drone Agent is local-first and field-deployed: every
inter-rig path is designed so the system keeps working when the operator has
no LAN, no Internet, and only a custom WFB radio link between the two rigs.

## The four layers

The drone and the ground station communicate across four discrete layers.
They are ordered by reliability in the field, not by deployment order.

### Layer 0 — WFB radio (primary)

The custom 5 GHz radio pair, RTL8812EU adapters in monitor mode, raw 802.11
frames managed by `wfb_tx` and `wfb_rx`. This is the only transport that is
guaranteed to be present in field conditions: the drone is in the air, the
ground station is on a tripod or in a vehicle, and there is no operator LAN
to depend on.

`wfb_tx` and `wfb_rx` accept a `-p <id>` argument that multiplexes multiple
streams on the same radio pair. The agent reserves three stream ids:

| Stream id | Direction notes | Use |
| --- | --- | --- |
| `-p 0` | Drone → GS (data path) | H.264 video, MAVLink downlink, MAVLink uplink (multiplexed) |
| `-p 1` | Bidirectional | Control plane: HopAnnounce + HopAck (FHSS coordination), PresenceBeacon |
| `-p 127` | Bidirectional, short-lived | Bind tunnel during pair |

Any packet whose loss breaks the live radio link MUST travel on Layer 0.
That includes the FHSS hop coordination frames and the periodic peer-presence
beacons. Both are designed to fit comfortably alongside the video stream on
the same radio pair: HopAnnounce is 51 bytes at most a few times per minute,
PresenceBeacon is 68 bytes every 10 seconds.

### Layer 1 — Cloud relay (fallback)

A Convex-backed MQTT-over-TLS + HTTPS heartbeat path through a hosted
relay. Used when the WFB radio link is broken, when the rig is in a hangar
or on a charging bench, and when Mission Control needs to keep a fleet
view of drones that are not on the same physical LAN as the operator.

The relay carries MAVLink and the heartbeat payload; it does not carry
video and it is not used for time-sensitive radio coordination. Cloud
latency and jitter make it unsuitable for replacing Layer 0.

### Layer 2 — Local LAN (convenience)

The operator's WiFi or Ethernet network. Used between Mission Control and
the ground station for the web UI, the setup webapp, plugin installs, and
log streaming. Layer 2 is never used drone-to-GS, because the operator's
LAN is typically absent or unreliable in the field. Removing the LAN does
not break inter-rig anything.

### Layer 3 — Offline / autonomous (degraded)

The drone executes a pre-uploaded mission plan without an active telemetry
channel back to the ground station. The ground station records locally and
queues fleet operations for later sync. Not implemented in the current
release; the architecture does not preclude it.

## Allowed and forbidden uses

| Channel | Layer | Forbidden on |
| --- | --- | --- |
| Pair / bind keypair exchange | Layer 0 (`-p 127`) | Layer 2 |
| Video downlink | Layer 0 (`-p 0`) | Layer 1, Layer 2 |
| MAVLink uplink / downlink | Layer 0 (`-p 0`) primary; Layer 1 fallback | Layer 2 |
| FHSS HopAnnounce / HopAck | Layer 0 (`-p 1`) | Layer 1, Layer 2 |
| Presence discovery | Layer 0 (`-p 1`) | Layer 1, Layer 2 |
| Heartbeat / status to fleet view | Layer 1 | Layer 0 |
| Mission Control GCS access | Layer 2 (operator-facing) or Layer 1 | Layer 0 |

If your new code talks rig-to-rig, ask: which layer is this on? If the
answer is Layer 2, you have a bug — the operator's LAN is the wrong place
for inter-rig coordination.

## Why the WFB radio is the canonical path

Three concrete failure modes pushed every drone-to-GS path onto Layer 0:

- **Consumer APs drop limited broadcasts.** `255.255.255.255` over the
  default route does not bridge from wired to wireless on most consumer
  APs. The drone broadcasts a HopAnnounce; the ground station never sees
  it; FHSS coordination breaks silently.
- **Subnet broadcasts depend on the subnet mask.** Hardcoding `/24` works
  on most deployments and fails on the rest. The agent is shipped to
  customers whose LAN topology we cannot predict.
- **mDNS name resolution adds a soft dependency on avahi-daemon and the
  operator's mDNS-friendly LAN.** Field crews don't always have one.

The WFB radio is the only path the agent fully controls end-to-end. If
the radio is up, the inter-rig link is up. If the radio is down, every
LAN workaround is theatre.

## Where in the code each layer lives

| Concern | File |
| --- | --- |
| `wfb_tx` / `wfb_rx` lifecycle, data plane `-p 0` | `src/ados/services/wfb/manager.py`, `src/ados/services/ground_station/wfb_rx.py` |
| Control-plane `-p 1` spawn (HopAnnounce + PresenceBeacon) | same files, `start_tx_control` / `start_rx_control` |
| HopSupervisor (drone) + HopListener (GS) | `src/ados/services/wfb/hop_supervisor.py` |
| PresenceBeacon dataclass | same file |
| Bind tunnel orchestration on `-p 127` | `src/ados/services/wfb/bind_orchestrator.py` |
| Cloud heartbeat + MQTT | `src/ados/services/cloud/` |
| Pair state on disk | `/etc/ados/config.yaml` (`video.wfb` section) |
| Peer presence on disk | `/run/ados/peer-presence.json` |
| FHSS hop snapshot on disk | `/run/ados/hop-supervisor.json` |

## Code review checklist

When introducing any inter-rig path, every one of these must be true:

- The path runs on Layer 0 (WFB radio) if it carries coordination,
  telemetry, or anything whose loss breaks the live link. Layer 1 is
  acceptable only for high-latency-tolerant fallbacks.
- The path is authenticated against replay and forgery. Use the same
  `_resolve_pair_key()` derivation that HopAnnounce and PresenceBeacon
  use, and seal frames with HMAC-SHA256 unless there is a documented
  reason to choose differently.
- The path does not depend on `255.255.255.255`, the LAN's `/24`
  subnet broadcast, `SO_BROADCAST`, or mDNS for delivery between the
  drone and the ground station.
- The path does not assume the operator has Internet access for
  drone-to-GS traffic. Internet is Layer 1 fallback only.
- The path tolerates the wfb-ng subprocess being restarted at any time
  (the TX-liveness watchdog, channel-hop commit, or operator-driven
  unpair can all cycle the subprocesses).
