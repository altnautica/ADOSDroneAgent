# ADOS Drone Agent

**Open-source onboard agent for software-defined drones. 50km data link. HD video. Full remote control.**

![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-green.svg) ![Python 3.11+](https://img.shields.io/badge/Python-3.11%2B-blue.svg) ![Status: Alpha](https://img.shields.io/badge/Status-Alpha-orange.svg) [![Discord](https://img.shields.io/badge/Discord-Join-5865F2.svg)](https://discord.gg/uxbvuD4d5q)

ADOS Drone Agent is the onboard intelligence layer for software-defined drones. It runs on your companion computer, proxies MAVLink from the flight controller to WebSocket and TCP, handles the 50km data link, streams HD video, and gives you full remote control from ADOS Mission Control or any HTTP client.

> **Pairs with [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl)** — open-source browser GCS with AI PID tuning, mission planning, 3D simulation, live ADS-B, and gamepad flight control at 50Hz.

<p align="center">
  <strong><a href="https://github.com/altnautica/ADOSMissionControl">ADOS Mission Control</a></strong> |
  <strong><a href="https://altnautica.com">Website</a></strong> |
  <strong><a href="https://discord.gg/uxbvuD4d5q">Discord</a></strong> |
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

---

## What It Does

**MAVLink proxy.** Reads the FC serial port and routes MAVLink to WebSocket, TCP, and UDP simultaneously. Multiple ground stations can connect at once. Auto-reconnect on FC disconnect.

**50km data link.** When paired with ADOS Mission Control, the agent publishes telemetry via MQTT over a Cloudflare Tunnel at 2Hz+. No port forwarding needed. Works from anywhere with a cellular connection.

**HD video streaming.** The video pipeline pushes an RTSP stream to a cloud relay. The GCS plays it in-browser via MediaSource Extensions at 0.5-1.5s latency. WFB-ng long-range video link support is planned.

**Full remote control.** The GCS can send arm/disarm, mode changes, guided flight commands, and mission uploads through the cloud relay. The agent polls and executes them. All from a browser, over any network.

**REST API.** FastAPI server at `:8080`. Get telemetry, set FC parameters, send commands, read logs — from any HTTP client or the paired GCS.

**Terminal dashboard.** Five-screen TUI via `ados tui`: overview, telemetry, MAVLink inspector, logs, config editor. SSH-friendly for headless hardware.

**Hardware auto-detection.** Detects board tier on boot (RPi Zero 2W through CM5 / Jetson) and enables services based on available resources.

---

## Hardware Support

| Tier | Hardware | RAM | Capabilities |
|------|----------|-----|-------------|
| Tier 1 (Basic) | RPi Zero 2W | 128MB+ | MAVLink proxy, MQTT gateway |
| Tier 2 (Smart) | RPi 4 / CM4 | 512MB+ | + Python scripting, sensor monitoring |
| Tier 3 (Autonomous) | CM5 / Jetson | 2GB+ | + Suite runtime, ROS2, vision, SLAM |
| Tier 4 (Swarm) | CM5 + radios | 2.5GB+ | + Mesh networking, formation flight |

Also runs on macOS in dev mode — useful for testing without a real drone.

---

## CLI Reference

| Command | Description |
|---------|-------------|
| `ados start` | Connect to FC and start all services |
| `ados demo` | Start with simulated telemetry (no hardware needed) |
| `ados tui` | Launch the terminal dashboard |
| `ados status` | FC connection and agent status |
| `ados health` | CPU, RAM, disk, temperature |
| `ados config show` | Print current config |
| `ados config set <key> <val>` | Update a config value |
| `ados mavlink status` | MAVLink proxy status and connected clients |
| `ados version` | Print agent version |

---

## REST API

FastAPI server at `:8080`. Full OpenAPI docs at `/docs`.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/status` | GET | Agent status, uptime, FC state |
| `/api/telemetry` | GET | Attitude, GPS, battery snapshot |
| `/api/params` | GET / PUT | Read or set FC parameters |
| `/api/commands` | POST | Send MAVLink command to FC |
| `/api/config` | GET / PUT | Read or update agent config |
| `/api/logs` | GET | Recent log entries |
| `/api/services` | GET | Running services and status |

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
│       REST API           │   FastAPI :8080
└────────────┬─────────────┘
             │
             ▼
┌──────────────────────────┐
│       AgentApp           │   Core process manager
└──┬──────────────┬────────┘
   │              │
   ▼              ▼
┌──────────┐  ┌──────────┐
│ MAVLink  │  │  MQTT    │   Services
│  Proxy   │  │ Gateway  │
└──────────┘  └──────────┘
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
| REST API (FastAPI, 7 route modules) | Working |
| TUI dashboard (5 screens) | Working |
| CLI (10 commands) | Working |
| Demo mode (simulated telemetry) | Working |
| Hardware detection (board YAML profiles) | Working |
| Config system (Pydantic + YAML) | Working |
| Health monitoring (CPU, RAM, disk, temp) | Working |
| MQTT gateway | Working |
| Cloud relay (Convex HTTP + MQTT) | Working |
| Video pipeline (HD, long-range link) | Planned |
| Suite runtime (YAML manifest execution) | Planned |
| Script executor (Python SDK, REST) | Planned |
| OTA updates | Planned |
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
