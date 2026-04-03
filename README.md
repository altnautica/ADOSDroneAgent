# ADOS Drone Agent

**Open-source onboard agent for software-defined drones. 50km data link. HD video. Full remote control.**

![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-green.svg) ![Python 3.11+](https://img.shields.io/badge/Python-3.11%2B-blue.svg) ![Status: Alpha](https://img.shields.io/badge/Status-Alpha-orange.svg) [![Discord](https://img.shields.io/badge/Discord-Join-5865F2.svg)](https://discord.gg/uxbvuD4d5q)

ADOS Drone Agent is the onboard intelligence layer for software-defined drones. It runs on your companion computer, proxies MAVLink from the flight controller to WebSocket and TCP, handles the 50km data link, streams HD video, and gives you full remote control from ADOS Mission Control or any HTTP client.

> **Part of the ADOS ecosystem.** Pairs with [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) (the browser GCS) for AI PID tuning, mission planning, 3D simulation, live ADS-B, and gamepad flight control at 50Hz. The agent runs on the drone; Mission Control runs in your browser.

<p align="center">
  <strong><a href="https://github.com/altnautica/ADOSMissionControl">ADOS Mission Control</a></strong> |
  <strong><a href="https://altnautica.com">Website</a></strong> |
  <strong><a href="https://discord.gg/uxbvuD4d5q">Discord</a></strong> |
  <strong><a href="mailto:team@altnautica.com">Email</a></strong> |
  <strong><a href="https://github.com/altnautica/ADOSDroneAgent/issues">Issues</a></strong>
</p>

---

<table>
  <tr>
    <td width="50%">
      <img src="docs/screenshots/overview.png" alt="ADOS Drone Agent overview showing services, system resources, and logs" height="220" width="100%"><br>
      <sub>Overview tab, showing running services, system resources, and live logs (<code>ados tui</code>)</sub>
    </td>
    <td width="50%">
      <img src="docs/screenshots/scripts.png" alt="Python script editor with syntax highlighting for drone automation" height="220" width="100%"><br>
      <sub>Script editor with syntax highlighting for Python drone automation</sub>
    </td>
  </tr>
  <tr>
    <td width="50%">
      <img src="docs/screenshots/fleet-network.png" alt="Fleet network enrollment, MQTT gateway, mesh radio peers" height="220" width="100%"><br>
      <sub>Fleet network enrollment, MQTT gateway status, and mesh radio peers</sub>
    </td>
    <td width="50%">
      <img src="docs/screenshots/suites.png" alt="Application suites: Sentry, Survey, Inspection, Agriculture, Cargo, SAR" height="220" width="100%"><br>
      <sub>Application suites: Sentry, Survey, Inspection, Agriculture, Cargo, and SAR</sub>
    </td>
  </tr>
</table>

<p align="center">
  <img src="docs/screenshots/peripherals.png" alt="Connected peripherals with live sensor readings" width="60%"><br>
  <sub>Connected peripherals with live sensor readings</sub>
</p>

---

## Quick Start

```bash
git clone https://github.com/altnautica/ADOSDroneAgent.git
cd ADOSDroneAgent
pip install -e ".[dev]"
ados demo    # simulated drone telemetry, no hardware needed
```

Deploy to a companion computer (Raspberry Pi, Jetson, etc.):

```bash
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh | bash
```

The script detects your OS, installs Python 3.11, auto-detects the FC serial port, and configures systemd services.

### System Requirements

| Requirement | Minimum | Recommended |
|-------------|---------|-------------|
| Python | 3.11+ | 3.12 |
| OS | Any Linux with systemd | Raspberry Pi OS, Ubuntu, Debian |
| RAM | 64MB (Tier 1 basic) | 512MB+ (Tier 2+) |
| Storage | 100MB | 500MB |
| FC connection | Serial (UART or USB) | UART at 921600 baud |

Also runs on macOS for local development and testing.

---

## Why ADOS Drone Agent

| | ADOS Drone Agent | Rpanion-server | BlueOS | Raw MAVProxy |
|---|---|---|---|---|
| **MAVLink proxy** | Yes (serial to WS/TCP/UDP) | Yes | Yes | Yes |
| **Cloud relay** | Yes (MQTT + Convex, zero port forwarding) | No | No | No |
| **HD video** | Yes (RTSP, WFB-ng planned) | Yes (basic) | Yes | No |
| **REST API** | Yes (15 route modules, OpenAPI docs) | Limited | Yes | No |
| **Terminal UI** | Yes (5 screens, SSH-friendly) | No | No | No |
| **Application suites** | Yes (6 YAML-based vertical modules) | No | No | No |
| **Hardware auto-detect** | Yes (tier-based feature scaling) | No | No | No |
| **OTA updates** | Planned | No | Yes | No |
| **Target** | Drones (any size) | Drones / Rovers | Underwater ROVs | Any MAVLink |
| **License** | GPL-3.0 | GPL-3.0 | Custom | GPL-3.0 |

---

## What It Does

**MAVLink proxy.** Reads the FC serial port and routes MAVLink to WebSocket, TCP, and UDP simultaneously. Multiple ground stations can connect at once. Auto-reconnect on FC disconnect.

**50km data link.** When paired with ADOS Mission Control, the agent publishes telemetry via MQTT over a Cloudflare Tunnel at 2Hz+. No port forwarding needed. Works from anywhere with a cellular connection.

**HD video streaming.** The video pipeline pushes an RTSP stream to a cloud relay. The GCS plays it in-browser via MediaSource Extensions at 0.5-1.5s latency. WFB-ng long-range video link support is planned.

**Full remote control.** The GCS can send arm/disarm, mode changes, guided flight commands, and mission uploads through the cloud relay. The agent polls and executes them. All from a browser, over any network.

**REST API.** FastAPI server at `:8080` with 15 route modules. Get telemetry, set FC parameters, send commands, manage config, control video, manage suites, run scripts. Full OpenAPI docs at `/docs`.

**Terminal dashboard.** Five-screen TUI via `ados tui`: overview, telemetry, MAVLink inspector, logs, config editor. SSH-friendly for headless hardware.

**Hardware auto-detection.** Detects board tier on boot (RPi Zero 2W through CM5 / Jetson) and enables services based on available resources.

---

## Hardware Support

| Tier | Hardware | RAM | Capabilities |
|------|----------|-----|-------------|
| Tier 1 (Basic) | RPi Zero 2W | 128MB+ | MAVLink proxy, MQTT gateway |
| Tier 2 (Smart) | RPi 4 / CM4 | 512MB+ | + Python scripting, sensor monitoring |
| Tier 3 (Autonomous) | CM5 / Jetson Nano | 2GB+ | + Suite runtime, ROS2, vision, SLAM |
| Tier 4 (Swarm) | CM5 + radios | 2.5GB+ | + Mesh networking, formation flight |

Any Linux ARM64 or x86_64 board with a serial port should work. The tier system scales features to available resources automatically.

---

## CLI Reference

24 commands. Run `ados --help` for the full list.

| Command | Description |
|---------|-------------|
| `ados start` | Connect to FC and start all services |
| `ados demo` | Start with simulated telemetry (no hardware needed) |
| `ados tui` | Launch the terminal dashboard |
| `ados status` | FC connection and agent status |
| `ados health` | CPU, RAM, disk, temperature |
| `ados config show` | Print current config |
| `ados config set <key> <val>` | Update a config value |
| `ados mavlink` | MAVLink proxy status and connected clients |
| `ados video` | Video pipeline status |
| `ados link` | Cloud connectivity status |
| `ados scripts` | List available automation scripts |
| `ados run <path>` | Execute a Python automation script |
| `ados send <command>` | Send a command to the FC (arm, disarm, mode) |
| `ados snap` | Take a camera snapshot |
| `ados pair` | Pair with ADOS Mission Control |
| `ados unpair` | Remove GCS pairing |
| `ados update` | Check for agent updates |
| `ados upgrade` | Upgrade to latest version |
| `ados rollback [version]` | Rollback to a previous version |
| `ados check` | Run pre-flight diagnostics |
| `ados uninstall` | Remove the agent |
| `ados version` | Print agent version |

---

## REST API

FastAPI server at `:8080`. Full OpenAPI docs at `/docs`. 15 route modules.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/status` | GET | Agent status, uptime, FC state |
| `/api/telemetry` | GET | Attitude, GPS, battery snapshot |
| `/api/params` | GET / PUT | Read or set FC parameters |
| `/api/commands` | POST | Send MAVLink command to FC |
| `/api/config` | GET / PUT | Read or update agent config |
| `/api/logs` | GET | Recent log entries |
| `/api/services` | GET | Running services and status |
| `/api/video` | GET / POST | Video pipeline status and control |
| `/api/scripts` | GET / POST | List and execute automation scripts |
| `/api/suites` | GET / PUT | Suite activation and status |
| `/api/fleet` | GET / POST | Fleet enrollment and network status |
| `/api/peripherals` | GET | Connected sensors and hardware |
| `/api/pairing` | GET / POST / DELETE | GCS pairing management |
| `/api/system` | GET / POST | System info, reboot, shutdown |
| `/api/ota` | GET / POST | Update check, upgrade, rollback |

```bash
# Get current telemetry
curl http://localhost:8080/api/telemetry

# Arm the drone
curl -X POST http://localhost:8080/api/commands \
  -H "Content-Type: application/json" \
  -d '{"command": "arm"}'
```

---

## Cloud Connectivity

The agent connects to ADOS Mission Control over a three-layer relay.

**Convex HTTP (baseline).** Every 5 seconds, the agent POSTs full status to the cloud. The GCS reads via reactive Convex queries. Commands go the reverse direction. Zero extra infra required.

**MQTT telemetry (real-time).** When `server.mode` is `cloud` or `self_hosted`, the agent publishes to `ados/{deviceId}/status` and `ados/{deviceId}/telemetry` via Mosquitto over WebSocket. The GCS subscribes in-browser via mqtt.js. 2Hz+ update rate.

**RTSP video.** The video pipeline pushes to a cloud relay, which converts it to fMP4-over-WebSocket for browser playback at 0.5-1.5s latency.

| Config field | Default | Description |
|---|---|---|
| `server.mode` | `disabled` | `disabled`, `cloud`, or `self_hosted` |
| `server.mqtt_transport` | `tcp` | `tcp` or `websockets` |
| `server.mqtt_username` | — | MQTT broker username |
| `video.cloud_relay_url` | — | RTSP relay server URL |

---

## Architecture

```
┌──────────┐  ┌──────────┐
│   CLI    │  │   TUI    │   User interfaces
└────┬─────┘  └────┬─────┘
     │              │
     ▼              ▼
┌──────────────────────────┐
│       REST API           │   FastAPI :8080 (15 route modules)
└────────────┬─────────────┘
             │
             ▼
┌──────────────────────────┐
│       AgentApp           │   Core process manager
└──┬───────┬───────┬───────┘
   │       │       │
   ▼       ▼       ▼
┌──────┐ ┌──────┐ ┌──────┐
│ MAV  │ │ MQTT │ │Video │   Services
│Proxy │ │ GW   │ │Pipe  │
└──────┘ └──────┘ └──────┘
   │
   ▼
┌──────────┐
│   FC     │   Flight controller (serial/USB)
└──────────┘
```

---

## What's Working

| Feature | Status |
|---------|--------|
| MAVLink proxy (serial to WS/TCP/UDP) | Working |
| REST API (FastAPI, 15 route modules) | Working |
| TUI dashboard (5 screens) | Working |
| CLI (24 commands) | Working |
| Demo mode (simulated telemetry) | Working |
| Hardware detection (board tier profiles) | Working |
| Config system (Pydantic + YAML) | Working |
| Health monitoring (CPU, RAM, disk, temp) | Working |
| MQTT gateway | Working |
| Cloud relay (Convex HTTP + MQTT) | Working |
| GCS pairing (Mission Control link) | Working |
| OTA updates (upgrade + rollback) | Working |
| Video pipeline (RTSP + cloud relay) | In Progress |
| WFB-ng long-range video link | Planned |
| Suite runtime (YAML manifest execution) | Planned |
| Script executor (Python SDK, REST) | Planned |
| Swarm coordination (mesh, formation) | Planned |

---

## Development

```bash
git clone https://github.com/altnautica/ADOSDroneAgent.git
cd ADOSDroneAgent
python -m venv .venv && source .venv/bin/activate
pip install -e ".[dev]"

pytest          # run tests
ruff check src/ # lint
ados demo       # run without hardware
ados tui        # launch terminal dashboard
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for code style and PR guidelines.

---

## Community

- **[Discord](https://discord.gg/uxbvuD4d5q)** — Ask questions, share builds
- **[Email](mailto:team@altnautica.com)** — team@altnautica.com
- **[Issues](https://github.com/altnautica/ADOSDroneAgent/issues)** — Bug reports and discussions
- **[Website](https://altnautica.com)** — Company and product info

---

## Related

- [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) — browser GCS (the control side of this pair)
- [ArduPilot](https://github.com/ArduPilot/ardupilot) — open-source autopilot firmware
- [OpenHD](https://github.com/OpenHD/OpenHD) — open-source digital FPV
- [WFB-ng](https://github.com/svpcom/wfb-ng) — WiFi broadcast for long-range video

---

## License

[GPL-3.0-only](LICENSE). Free to use, modify, and distribute. Derivative works must also be GPL-3.0.
