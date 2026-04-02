# ADOS Ground Station — Documentation

The ADOS Ground Station is a companion product to the ADOS ADOS Drone Agent air unit. It receives WFB-ng long-range video and telemetry from the drone, and relays it to any browser (Mac, Windows, phone) via WiFi AP and WebRTC.

Same codebase as ADOS Drone Agent, running in RX mode instead of TX mode. Same HGLRC baseboard hardware (DEC-073) can serve both products.

## Reading Order

| # | Document | Purpose |
|---|----------|---------|
| 1 | [architecture.md](architecture.md) | Software design, service layout, data flow |
| 2 | [hardware.md](hardware.md) | Hardware variants, BOM, baseboard reuse |
| 3 | [user-experience.md](user-experience.md) | Setup flow, pairing, browser connection |
| 4 | [wfb-ng-guide.md](wfb-ng-guide.md) | WFB-ng deep dive: monitor mode, FEC, encryption, RTL8812EU |
| 5 | [antenna-guide.md](antenna-guide.md) | Antenna types, gain, range, diversity, regulatory limits |
| 6 | [competitive-landscape.md](competitive-landscape.md) | OpenHD, RubyFPV, Herelink, SIYI, Skydroid comparison |
| 7 | [platform-compatibility.md](platform-compatibility.md) | OS support matrix, why hardware GS is needed |

## Key Concept

WFB-ng (WiFi Broadcast) requires Linux with monitor mode drivers. Mac, Windows, Android, and iOS cannot run it natively. The ADOS Ground Station solves this by running Linux internally and exposing video/telemetry via WiFi AP + browser. Users never touch Linux.
