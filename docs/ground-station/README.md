# ADOS Ground Station — Documentation

The ADOS Ground Station is a companion product to the ADOS Drone Agent air unit. It receives WFB-ng long-range video and telemetry from the drone, and relays it to any browser (Mac, Windows, phone) via WiFi AP and WebRTC.

Same codebase as ADOS Drone Agent, running in RX mode instead of TX mode. Same reference baseboard hardware can serve both products.

The ground station can optionally run in one of three roles: `direct` (the default: one node serves the pilot end-to-end), `relay` (a field-placed node that forwards received WFB-ng fragments to the receiver over a local wireless mesh), or `receiver` (the hub that combines fragments from one or more relays into one clean stream using WFB-ng's native FEC). `direct` is the current default and covers the one-pilot, one-site case. `relay` and `receiver` are opt-in when a deployment needs coverage across obstructions or long corridors.

## Reading Order

| # | Document | Purpose |
|---|----------|---------|
| 1 | [architecture.md](architecture.md) | Software design, service layout, data flow, multi-node topology |
| 2 | [hardware.md](hardware.md) | Hardware variants, BOM, baseboard reuse, per-role deltas |
| 3 | [user-experience.md](user-experience.md) | Setup flow, pairing, browser connection, field-only tap-to-pair |
| 4 | [wfb-ng-guide.md](wfb-ng-guide.md) | WFB-ng deep dive: monitor mode, FEC, encryption, RTL8812EU, distributed RX |
| 5 | [antenna-guide.md](antenna-guide.md) | Antenna types, gain, range, diversity, regulatory limits |
| 6 | [platform-compatibility.md](platform-compatibility.md) | OS support matrix, why hardware GS is needed |
| 7 | [mesh-networking.md](mesh-networking.md) | batman-adv transport between relay and receiver |
| 8 | [relay-mode-setup.md](relay-mode-setup.md) | Set up a relay node step by step |
| 9 | [receiver-mode-setup.md](receiver-mode-setup.md) | Set up a receiver hub step by step |
| 10 | [pairing-protocol.md](pairing-protocol.md) | Field tap-to-pair protocol internals |
| 11 | [cli-reference-mesh.md](cli-reference-mesh.md) | Full `ados gs role` and `ados gs mesh` reference |
| 12 | [rest-api-mesh.md](rest-api-mesh.md) | REST surface for role, mesh, and pairing |

## Key Concept

WFB-ng (WiFi Broadcast) requires Linux with monitor mode drivers. Mac, Windows, Android, and iOS cannot run it natively. The ADOS Ground Station solves this by running Linux internally and exposing video/telemetry via WiFi AP + browser. Users never touch Linux.

## Related

- Public operator docs: [docs.altnautica.com/ground-agent/mesh-overview](https://docs.altnautica.com/ground-agent/mesh-overview)
