# ADOS Drone Agent

**Software-defined drone network agent. Install on any companion computer to make your drone smart, autonomous, and fleet-ready.**

ADOS Drone Agent is the onboard software layer for drones. It runs on a companion computer (Raspberry Pi CM4/CM5, Jetson, or any Linux SBC), connects to the flight controller via MAVLink, and turns a basic drone into a smart, networked, autonomous platform.

Think of it as the **Android for drones**: open-source, modular, and works with any hardware.

```
┌─────────────────────────────────────────────────────────────┐
│                    ADOS Drone Agent                          │
├─────────────────────────────────────────────────────────────┤
│  Layer 6 │ Suite Runtime + Script Executor    (application) │
│  Layer 5 │ Plugin System                      (extensibility)│
│  Layer 4 │ MAVLink Proxy + Video + MQTT       (connectivity)│
│  Layer 3 │ Hardware Abstraction Layer          (platform)   │
│  Layer 2 │ Security + OTA + Install            (infra)      │
│  Layer 1 │ Process Supervisor + systemd        (core)       │
│  Layer 0 │ Linux OS                            (base)       │
└─────────────────────────────────────────────────────────────┘
```

## What It Does

- **MAVLink Proxy** - Routes serial FC data to WebSocket, UDP, MQTT. Supports multiple GCS connections simultaneously.
- **Video Link** - Manages WFB-ng (same tech as OpenHD/RubyFPV) for long-range HD video. 50km+ range with hardware encoding.
- **Suite Runtime** - Loads modular suite packages (Sentry, Survey, Agriculture, Cargo, SAR, Inspection) from YAML manifests. Activates sensors, starts ROS2 nodes, and configures mission templates.
- **Fleet Management** - Connects to DroneNet for cloud fleet tracking, mission dispatch, and telemetry aggregation. Works with Altnautica cloud or your own self-hosted server.
- **Scripting** - 5 tiers from simple text commands (`takeoff`, `forward 100`) to Python SDK, YAML missions, REST API, and Blockly visual programming.
- **Swarm Coordination** - Mesh networking via LoRa + WiFi Direct. Formation flight, leader election, task decomposition.
- **OTA Updates** - A/B partition scheme with automatic rollback. Stable/beta/nightly channels. Delta updates.
- **Plugin System** - Extend the agent with Python packages. Standard entry points, resource sandboxing, event bus.

## Hardware Support

The agent auto-detects hardware at boot and enables services based on available resources.

| Tier | Hardware | RAM | Capabilities |
|------|----------|-----|-------------|
| Tier 1 (Basic) | RPi Zero 2W | 128MB+ | MAVLink proxy, MQTT gateway |
| Tier 2 (Smart) | RPi 4 / CM4 | 512MB+ | + Python scripting, sensor monitoring |
| Tier 3 (Autonomous) | CM5 / Jetson | 2GB+ | + Suite runtime, ROS2, vision, SLAM |
| Tier 4 (Swarm) | CM5 + radios | 2.5GB+ | + Mesh networking, formation flight |

Supported boards: Raspberry Pi CM4, Raspberry Pi CM5, Raspberry Pi 4/5, Jetson Nano, Jetson Orin Nano, Radxa CM3/CM5, any Linux SBC with UART/USB.

## Quick Start

### Install on a companion computer

```bash
curl -sSL https://install.ados.altnautica.com | bash
```

The install script will:
1. Detect your OS (Raspberry Pi OS, Ubuntu, Armbian)
2. Install Python 3.11 and dependencies
3. Auto-detect the flight controller serial port
4. Configure WiFi link (WFB-ng or client mode)
5. Generate a device certificate
6. Register with DroneNet (or your self-hosted server)
7. Enable and start systemd services

### Configuration

Edit `/etc/ados/config.yaml` to customize. See `configs/config.example.yaml` for all options.

### Connect from GCS

Open ADOS Mission Control in your browser. Go to the **Command** tab. Your drone should appear automatically if on the same network.

## Scripting

### Text commands (Tello-style)

```
$ ados send takeoff
OK
$ ados send forward 100
OK
$ ados send battery?
87
$ ados send land
OK
```

### Python SDK

```python
from ados import ADOSDrone

async def main():
    drone = ADOSDrone("192.168.1.100")
    await drone.connect()
    await drone.takeoff(10)
    await drone.forward(100)
    await drone.rotate(90)
    await drone.land()
```

### REST API

```bash
curl http://192.168.1.100:8080/status
curl -X POST http://192.168.1.100:8080/command -d '{"cmd": "takeoff", "alt": 10}'
```

### YAML Missions

```yaml
mission:
  name: "Survey Grid"
  suite: survey
  waypoints:
    - lat: 12.9716
      lon: 77.5946
      alt: 50
      action: photo
    - lat: 12.9720
      lon: 77.5950
      alt: 50
      action: photo
  on_complete: rtl
```

## Suite Modules

Built-in suites provide mission-specific capabilities:

| Suite | Purpose | Key Sensors |
|-------|---------|-------------|
| Sentry | Patrol, surveillance | RGB camera, thermal (optional) |
| Survey | Mapping, photogrammetry | RGB camera, LiDAR (optional) |
| Agriculture | Crop monitoring, spraying | Multispectral, spray controller |
| Cargo | Delivery, logistics | Weight sensor, release mechanism |
| SAR | Search and rescue | Thermal camera, spotlight |
| Inspection | Structural assessment | Zoom camera, thermal |

Suites are YAML manifests that declare required sensors, ROS2 launch profiles, mission templates, and dashboard widgets. Install via the Module Store in ADOS Mission Control GCS.

## Deployment Models

| Model | Description | Best For |
|-------|-------------|----------|
| **Cloud** | Connects to Altnautica MQTT broker. Zero setup. | Hobbyists, small operators |
| **Self-Hosted** | Your own MQTT broker + fleet API (Docker Compose provided). Full data sovereignty. | Enterprise, military |
| **Hybrid** | Both cloud (community features, updates) and private server (operational data). | Organizations with compliance needs |

## Project Structure

```
src/
  core/           # Process supervisor, config, logging, health monitoring
  services/
    mavlink/      # MAVLink proxy and serial-to-network routing
    video/        # WFB-ng video pipeline and camera management
    mqtt/         # MQTT gateway for fleet telemetry
    suite/        # Suite runtime (YAML manifest execution, ROS2)
    script/       # Script executor (text commands, Python scripts)
    ota/          # Over-the-air update manager (A/B partition)
    sensor/       # Sensor discovery, driver management, calibration
  plugins/        # Plugin system (Python entry points)
  sdk/            # Python SDK (ados package)
  api/            # REST API (FastAPI, OpenAPI 3.0)
tests/            # Test suite
configs/          # Example configuration files
docs/             # Documentation
scripts/          # Install scripts, utilities
```

## Development

```bash
git clone https://github.com/altnautica/ADOSDroneAgent.git
cd ADOSDroneAgent
python -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"
pytest
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for code style, PR process, and architecture details.

## Status

**Phase 0: Research and Documentation** (current)

The agent is in the design phase. Architecture specs, competitor audit, scripting language design, and UI/UX concepts are being finalized. No implementation code yet.

See the [development roadmap](https://github.com/altnautica/ADOSDroneAgent/wiki/Roadmap) for the full plan.

## Related Projects

- [ADOS Mission Control](https://github.com/altnautica/ADOSMissionControl) - Open-source web GCS for ArduPilot drones
- [ArduPilot](https://github.com/ArduPilot/ardupilot) - Open-source autopilot
- [OpenHD](https://github.com/OpenHD/OpenHD) - Open-source digital FPV
- [WFB-ng](https://github.com/svpcom/wfb-ng) - WiFi broadcast for FPV

## License

[GPLv3](LICENSE) - Free to use, modify, and distribute. Contributions welcome.

## Community

- Issues: [github.com/altnautica/ADOSDroneAgent/issues](https://github.com/altnautica/ADOSDroneAgent/issues)
- ADOS Mission Control community: [command.altnautica.com/community](https://command.altnautica.com/community)
- Website: [altnautica.com](https://altnautica.com)
