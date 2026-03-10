# ADOS Drone Agent

**Open-source onboard agent for autonomous drones. Runs on any companion computer, connects to your flight controller, exposes a REST API and TUI dashboard.**

ADOS Drone Agent sits between your flight controller and the network. It runs on a Raspberry Pi CM4/CM5, Jetson, or any Linux SBC (and macOS for development). It proxies MAVLink from the FC serial port to WebSocket, TCP, and UDP endpoints so multiple ground stations can connect at once. It also provides a REST API for remote management, a terminal dashboard for quick monitoring, and a CLI for scripting and automation.

> **Status: Alpha** (3,000+ LOC Python, actively developed)

## Quick Start

```bash
git clone https://github.com/altnautica/ADOSDroneAgent.git
cd ADOSDroneAgent
pip install -e ".[dev]"
ados demo    # runs simulated drone telemetry, no hardware needed
```

## Architecture

```
┌──────────┐  ┌──────────┐
│   CLI    │  │   TUI    │   User interfaces
└────┬─────┘  └────┬─────┘
     │             │
     ▼             ▼
┌──────────────────────────┐
│       REST API           │   FastAPI :8080
│  (status, telemetry,     │
│   params, commands,      │
│   config, logs,          │
│   services)              │
└────────────┬─────────────┘
             │
             ▼
┌──────────────────────────┐
│       AgentApp           │   Core process manager
│  ┌────────┐ ┌─────────┐ │
│  │ Health │ │ Config  │ │
│  └────────┘ └─────────┘ │
└──┬──────────────┬────────┘
   │              │
   ▼              ▼
┌──────────┐  ┌──────────┐
│ MAVLink  │  │  MQTT    │   Services
│  Proxy   │  │ Gateway  │
│ (serial  │  │(optional)│
│  → WS,   │  └──────────┘
│  TCP,UDP)│
└──────────┘
   │
   ▼
┌──────────┐
│   FC     │   Flight controller (serial/USB)
└──────────┘
```

## CLI Reference

| Command | Description |
|---------|-------------|
| `ados version` | Print agent version |
| `ados status` | Show agent and FC connection status |
| `ados health` | Show system health (CPU, RAM, disk, temp) |
| `ados start` | Start the agent (connects to FC, starts API) |
| `ados demo` | Start with simulated drone telemetry (no FC needed) |
| `ados tui` | Launch the terminal dashboard |
| `ados config show` | Print current configuration |
| `ados config get <key>` | Get a specific config value |
| `ados config set <key> <val>` | Set a config value |
| `ados mavlink status` | Show MAVLink proxy status and connected clients |

## TUI Dashboard

Launch with `ados tui`. Five screens, switch with number keys:

| Screen | Key | What it shows |
|--------|-----|---------------|
| Dashboard | `1` | System overview: FC status, GPS, battery, mode, armed state |
| Telemetry | `2` | Live attitude, position, velocity, battery cells |
| MAVLink Inspector | `3` | Raw MAVLink message stream with filtering |
| Logs | `4` | Agent log viewer with level filtering |
| Config Editor | `5` | Browse and edit YAML config in the terminal |

## REST API

FastAPI server runs at `:8080`. All endpoints return JSON.

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/status` | GET | Agent status, uptime, FC connection state |
| `/api/telemetry` | GET | Current telemetry snapshot (attitude, GPS, battery) |
| `/api/config` | GET | Current agent configuration |
| `/api/config` | PUT | Update configuration values |
| `/api/logs` | GET | Recent log entries (supports level filter) |
| `/api/services` | GET | Running services and their status |
| `/api/params` | GET | FC parameters (cached) |
| `/api/params` | PUT | Set FC parameter value |
| `/api/commands` | POST | Send MAVLink command to FC |

Example:

```bash
# Get agent status
curl http://localhost:8080/api/status

# Get current telemetry
curl http://localhost:8080/api/telemetry

# Send a command
curl -X POST http://localhost:8080/api/commands \
  -H "Content-Type: application/json" \
  -d '{"command": "arm"}'
```

## Hardware Support

The agent auto-detects hardware at boot and enables services based on available resources.

| Tier | Hardware | RAM | Capabilities |
|------|----------|-----|-------------|
| Tier 1 (Basic) | RPi Zero 2W | 128MB+ | MAVLink proxy, MQTT gateway |
| Tier 2 (Smart) | RPi 4 / CM4 | 512MB+ | + Python scripting, sensor monitoring |
| Tier 3 (Autonomous) | CM5 / Jetson | 2GB+ | + Suite runtime, ROS2, vision, SLAM |
| Tier 4 (Swarm) | CM5 + radios | 2.5GB+ | + Mesh networking, formation flight |

**Detected boards:** Raspberry Pi CM4, Raspberry Pi CM5, Jetson (generic), macOS (dev mode), generic ARM64 Linux. Board profiles are YAML files in `src/ados/hal/boards/`.

On macOS, the agent runs in dev mode with simulated hardware detection. Good for development and testing without a real drone.

## Project Structure

```
src/ados/
  __init__.py
  core/               # AgentApp, config (Pydantic + YAML), logging (structlog),
                      #   health monitoring, defaults
  cli/                # Click CLI (ados command)
  api/                # FastAPI server
    routes/           # Route modules: status, telemetry, config, logs,
                      #   services, params, commands
  hal/                # Hardware abstraction layer
    detect.py         # Auto-detect board, assign tier
    boards/           # Board profiles (cm4.yaml, cm5.yaml, generic-arm64.yaml)
  tui/                # Textual TUI app
    screens/          # dashboard, telemetry, mavlink, logs, config_editor
  services/
    mavlink/          # FC connection (serial/USB), MAVLink state machine,
                      #   message streams, WebSocket proxy, TCP proxy (5760),
                      #   UDP proxy (14550/14551), demo mode
    mqtt/             # MQTT gateway (paho-mqtt, optional)
configs/              # config.example.yaml, defaults.yaml
tests/                # pytest suite (api, config, connection, demo, hal, proxy, state)
scripts/              # install.sh (Linux + macOS)
.github/workflows/    # CI (ci.yml) + PyPI publish (publish.yml)
```

## What's Implemented vs Planned

| Feature | Status | Notes |
|---------|--------|-------|
| MAVLink proxy (serial to WS/TCP/UDP) | **Working** | Multi-client routing, reconnect logic |
| REST API (FastAPI) | **Working** | 7 route modules, OpenAPI docs at /docs |
| TUI dashboard (Textual) | **Working** | 5 screens, live updates |
| CLI | **Working** | 10 commands, Click-based |
| Demo mode | **Working** | Simulated telemetry, no hardware needed |
| Hardware detection (HAL) | **Working** | Board YAML profiles, tier assignment |
| Config system | **Working** | Pydantic models, YAML, deep merge, auto device_id |
| Health monitoring | **Working** | CPU, RAM, disk, temp, systemd watchdog |
| Structured logging | **Working** | structlog, JSON output, configurable level |
| MQTT gateway | **Working** | paho-mqtt, optional (disabled in demo) |
| Board profiles | **Working** | CM4, CM5, generic ARM64 |
| Video pipeline (WFB-ng) | Planned | HD video link management |
| Suite runtime | Planned | YAML manifest execution, ROS2 integration |
| Script executor | Planned | Text commands, Python SDK |
| OTA updates | Planned | A/B partition, rollback |
| Swarm coordination | Planned | LoRa mesh, formation flight |
| Plugin system | Planned | Python entry points |

## Development

```bash
# Clone and install
git clone https://github.com/altnautica/ADOSDroneAgent.git
cd ADOSDroneAgent
python -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"

# Run tests
pytest

# Run linter
ruff check src/

# Run in demo mode (no hardware)
ados demo

# Launch TUI
ados tui
```

### Linux Install Script

For deploying on a companion computer (Raspberry Pi, Jetson, etc.):

```bash
curl -sSL https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install.sh | bash
```

The script detects your OS, installs Python 3.11 and dependencies, auto-detects the FC serial port, and configures systemd services.

See [CONTRIBUTING.md](CONTRIBUTING.md) for code style, PR process, and architecture details.

## Related Projects

- [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) . Open-source web GCS for ArduPilot, PX4, and Betaflight drones
- [ArduPilot](https://github.com/ArduPilot/ardupilot) . Open-source autopilot firmware
- [OpenHD](https://github.com/OpenHD/OpenHD) . Open-source digital FPV
- [WFB-ng](https://github.com/svpcom/wfb-ng) . WiFi broadcast for long-range video

## License

[GPLv3](LICENSE). Free to use, modify, and distribute. Contributions welcome.

## Community

- Issues: [github.com/altnautica/ADOSDroneAgent/issues](https://github.com/altnautica/ADOSDroneAgent/issues)
- Discord: [discord.gg/uxbvuD4d5q](https://discord.gg/uxbvuD4d5q)
- Website: [altnautica.com](https://altnautica.com)
