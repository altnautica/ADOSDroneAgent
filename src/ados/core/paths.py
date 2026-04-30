"""Centralized filesystem path constants for ADOS Drone Agent.

All on-disk locations the agent reads from or writes to are declared
here. Other modules import these constants instead of hardcoding string
literals so that runtime layout changes can be made in one place.

Three top-level directories are used:

* ``/run/ados/``  runtime sockets, pid files, ephemeral live state.
* ``/etc/ados/``  persistent configuration written by the operator,
  installer, or pairing flow.
* ``/var/ados/``  persistent agent-owned data such as recordings, OTA
  state, logs, and downloaded assets.

This module is a leaf: it imports nothing from other ``ados.*``
modules and is safe to import from anywhere.
"""

from pathlib import Path

# ---------------------------------------------------------------------------
# Runtime directory: /run/ados/
# Sockets, pid files, ephemeral state. Wiped on reboot by tmpfs.
# ---------------------------------------------------------------------------

ADOS_RUN_DIR = Path("/run/ados")

# IPC sockets
MAVLINK_SOCK = ADOS_RUN_DIR / "mavlink.sock"
STATE_SOCK = ADOS_RUN_DIR / "state.sock"
MESH_SOCK = ADOS_RUN_DIR / "mesh.sock"
PAIRING_SOCK = ADOS_RUN_DIR / "pairing.sock"

# Live JSON state snapshots
HEALTH_JSON = ADOS_RUN_DIR / "health.json"
MESH_STATE_JSON = ADOS_RUN_DIR / "mesh-state.json"
WFB_RELAY_JSON = ADOS_RUN_DIR / "wfb-relay.json"
WFB_RECEIVER_JSON = ADOS_RUN_DIR / "wfb-receiver.json"

# Sentinel files
UPLINK_ACTIVE_FLAG = ADOS_RUN_DIR / "uplink-active"
AP_WAS_ENABLED_FLAG = ADOS_RUN_DIR / "ap-was-enabled"

# USB gadget composer runtime artifacts
DNSMASQ_USB0_CONF = ADOS_RUN_DIR / "dnsmasq-usb0.conf"
DNSMASQ_USB0_PID = ADOS_RUN_DIR / "dnsmasq-usb0.pid"

# ---------------------------------------------------------------------------
# Config directory: /etc/ados/
# Persistent operator-owned configuration. Written by the installer,
# the pairing flow, and the REST API.
# ---------------------------------------------------------------------------

ADOS_ETC_DIR = Path("/etc/ados")

# Top-level config + identity
CONFIG_YAML = ADOS_ETC_DIR / "config.yaml"
DEVICE_ID_PATH = ADOS_ETC_DIR / "device-id"
PAIRING_JSON = ADOS_ETC_DIR / "pairing.json"
PROFILE_CONF = ADOS_ETC_DIR / "profile.conf"
BOARD_OVERRIDE_PATH = ADOS_ETC_DIR / "board_override"
ENV_FILE = ADOS_ETC_DIR / "env"
FIREWALL_RULES_PATH = ADOS_ETC_DIR / "firewall.rules"
AP_PASSPHRASE_PATH = ADOS_ETC_DIR / "ap-passphrase"

# Hostapd + dnsmasq config files (rendered on demand)
HOSTAPD_CONF_PATH = ADOS_ETC_DIR / "hostapd-gs.conf"
DNSMASQ_CONF_PATH = ADOS_ETC_DIR / "dnsmasq-gs.conf"

# Ground-station side-files (legacy + active migrations)
GS_UI_JSON = ADOS_ETC_DIR / "ground-station-ui.json"
GS_UPLINK_JSON = ADOS_ETC_DIR / "ground-station-uplink.json"
GS_INPUT_JSON = ADOS_ETC_DIR / "ground-station-input.json"
GS_MODEM_JSON = ADOS_ETC_DIR / "ground-station-modem.json"
GS_WIFI_CLIENT_JSON = ADOS_ETC_DIR / "ground-station-wifi-client.json"

# Suites
SUITES_DIR = ADOS_ETC_DIR / "suites"

# Peripherals
PERIPHERALS_DIR = ADOS_ETC_DIR / "peripherals"
PERIPHERALS_GLOB = "/etc/ados/peripherals/*.yaml"

# Plugins
PLUGIN_KEYS_DIR = ADOS_ETC_DIR / "plugin-keys"
PLUGIN_REVOCATIONS_PATH = ADOS_ETC_DIR / "plugin-revocations.json"
PLUGIN_RUN_DIR = ADOS_RUN_DIR / "plugins"
PLUGIN_UNIT_DIR = Path("/etc/systemd/system")
PLUGIN_UNIT_PREFIX = "ados-plugin-"

# TLS certificates
CERTS_DIR = ADOS_ETC_DIR / "certs"
DEVICE_CERT_PATH = CERTS_DIR / "device.crt"
DEVICE_KEY_PATH = CERTS_DIR / "device.key"
CA_CERT_PATH = CERTS_DIR / "ca.crt"

# Mesh
MESH_DIR = ADOS_ETC_DIR / "mesh"
MESH_ID_PATH = MESH_DIR / "id"
MESH_PSK_PATH = MESH_DIR / "psk.key"
MESH_ROLE_PATH = MESH_DIR / "role"
MESH_GATEWAY_JSON = MESH_DIR / "gateway.json"
MESH_RECEIVER_JSON = MESH_DIR / "receiver.json"
MESH_REVOCATIONS_JSON = MESH_DIR / "revocations.json"

# WFB-ng key material
WFB_KEY_DIR = ADOS_ETC_DIR / "wfb"
WFB_RX_KEY_PATH = WFB_KEY_DIR / "rx.key"
WFB_RX_KEY_PUB_PATH = WFB_KEY_DIR / "rx.key.pub"

# ---------------------------------------------------------------------------
# Data directory: /var/ados/
# Agent-owned persistent data. Recordings, OTA state, logs, downloads.
# ---------------------------------------------------------------------------

ADOS_VAR_DIR = Path("/var/ados")

# Recordings + media
RECORDINGS_DIR = ADOS_VAR_DIR / "recordings"

# Flight logs
FLIGHT_LOGS_DIR = ADOS_VAR_DIR / "logs/flights"

# Scripts (user-loaded scripting payloads)
SCRIPTS_DIR = ADOS_VAR_DIR / "scripts"

# OTA
DOWNLOADS_DIR = ADOS_VAR_DIR / "downloads"
OTA_STATE_PATH = ADOS_VAR_DIR / "ota-state.json"
SLOT_A_PATH = ADOS_VAR_DIR / "slot-a"
SLOT_B_PATH = ADOS_VAR_DIR / "slot-b"

# Suite activation state
STATE_DIR = ADOS_VAR_DIR / "state"
ACTIVE_SUITE_PATH = STATE_DIR / "active_suite"

# ROS recordings + compose file
ROS_DIR = ADOS_VAR_DIR / "ros"
ROS_COMPOSE_PATH = ROS_DIR / "docker-compose.yml"
ROS_RECORDINGS_DIR = ROS_DIR / "recordings"

# Audit log
AUDIT_LOG_PATH = ADOS_VAR_DIR / "audit.jsonl"

# Plugins (installed third-party bundles, plugin data, plugin configs)
PLUGINS_INSTALL_DIR = ADOS_VAR_DIR / "plugins"
PLUGIN_DATA_DIR = ADOS_VAR_DIR / "plugin-data"
PLUGIN_LOG_DIR = Path("/var/log/ados/plugins")
PLUGIN_STATE_PATH = STATE_DIR / "plugin-state.json"
