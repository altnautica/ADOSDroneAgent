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
# Operator radio-knob command socket served by the native transmit plane
# (ados-radio). The REST layer forwards FEC/MCS/TX-power/link-tier changes
# here when the native radio is the running implementation; the packaged
# Python manager owns the same knobs in-process otherwise.
WFB_CMD_SOCK = ADOS_RUN_DIR / "wfb-cmd.sock"

# Ingest socket for the local logging and telemetry store. The store's
# writer process binds this; every producer (the native services and this
# Python agent) connects and ships length-prefixed msgpack frames. The
# socket is absent when the store is not installed or not yet started,
# which is the normal state on a fresh box: producers degrade to their
# secondary sink (stderr/journald) and retry the connection on a backoff.
LOGD_INGEST_SOCK = ADOS_RUN_DIR / "logd.sock"

# Query socket for the local logging and telemetry store. The store binds
# this trusted local plane (0o660, tmpfs) and serves the read API on it with
# no auth — anything on-box that can open the socket is already inside the
# trust boundary. The `ados logs` CLI and the FastAPI reverse-proxy bridge
# both prefer it because it answers even when the FastAPI surface on :8080 is
# down. Absent until the store is installed and started.
LOGD_QUERY_SOCK = ADOS_RUN_DIR / "logd-query.sock"

# Trigger seam for an explicit, operator-initiated cloud export of a chosen log
# window. The thin Python front door (the `ados logs push` CLI and the
# `/api/logs/push` endpoint) writes the request file; the long-running cloud
# service watches for it, performs the export-and-mark, then deletes the request
# and writes the result file for the front door to read back. The window export,
# upload, and mark-synced steps all live in the cloud service, not here: the
# Python side only signals intent and reports the outcome.
LOGD_PUSH_REQUEST_PATH = ADOS_RUN_DIR / "logd-push-request.json"
LOGD_PUSH_RESULT_PATH = ADOS_RUN_DIR / "logd-push-result.json"

# Live JSON state snapshots
HEALTH_JSON = ADOS_RUN_DIR / "health.json"
MESH_STATE_JSON = ADOS_RUN_DIR / "mesh-state.json"
WFB_RELAY_JSON = ADOS_RUN_DIR / "wfb-relay.json"
WFB_RECEIVER_JSON = ADOS_RUN_DIR / "wfb-receiver.json"
# Cross-process mesh-event journal. When the relay/receiver loops run in their
# own process (the native data-plane binary), they cannot reach the in-process
# asyncio mesh event bus, so they append newline-delimited JSON events here.
# The mesh-event tailer follows this file and republishes each line onto the
# in-process bus so the REST WebSocket + OLED light up unchanged.
MESH_EVENTS_JSONL = ADOS_RUN_DIR / "mesh-events.jsonl"

# Live wfb-ng radio stats snapshot (rssi, snr, packets, fec, bitrate).
# Written ~once per second by whichever wfb manager owns the radio:
# WfbManager on the drone profile, WfbRxManager on the GS profile.
# Read by the API layer + the OLED dashboard tile + the LCD link
# stats page. The cross-process file is the right shape because the
# wfb subprocess and the api subprocess don't share memory and the
# wfb stats need to surface to multiple consumers per box.
WFB_STATS_JSON = ADOS_RUN_DIR / "wfb-stats.json"

# Hop supervisor + bitrate controller snapshots. Both live inside the
# ados-wfb service in production multi-process; consumers (api,
# oled, lcd channel-hops page) read these files because the
# accessors are cross-process-blind. Written by their owners every
# ~5 s (atomic tmpfile+rename).
HOP_SUPERVISOR_JSON = ADOS_RUN_DIR / "hop-supervisor.json"
PEER_PRESENCE_JSON = ADOS_RUN_DIR / "peer-presence.json"
CAMERA_STATE_JSON = ADOS_RUN_DIR / "camera-state.json"
# Management-link health, written by the supervisor's management-link guardian
# each tick: the operator's management link state + repair-ladder progress.
MGMT_LINK_JSON = ADOS_RUN_DIR / "mgmt-link.json"
# Management-link reach-back mode, written by the supervisor's heartbeat-failover
# reconciler: primary / wifi_heartbeat / none when the wired primary is down.
MGMT_FAILOVER_JSON = ADOS_RUN_DIR / "mgmt-failover.json"
BITRATE_CONTROLLER_JSON = ADOS_RUN_DIR / "bitrate-controller.json"

# Local-bind to cloud-relay failover state. Written by the always-on
# auto-pair supervisor (a separate process from the API) when a fresh
# rig keeps failing to bind locally and falls back to the cloud relay.
# Read by GET /api/wfb/pair/failover-status. Single ``{"state": ...}``
# JSON object, atomic write, mode 0o644; default ``local`` when absent.
WFB_FAILOVER_STATE_JSON = ADOS_RUN_DIR / "wfb_failover.json"

# Sentinel files
UPLINK_ACTIVE_FLAG = ADOS_RUN_DIR / "uplink-active"
AP_WAS_ENABLED_FLAG = ADOS_RUN_DIR / "ap-was-enabled"

# Radio-module-source breadcrumb. Written by the install pipeline to
# record whether the WFB kernel module came from a prebuilt package or
# a DKMS build. Lives on tmpfs so it disappears on reboot; the heartbeat
# treats it as a fast hint and prefers the live modinfo path as the
# authoritative source. Values: "prebuilt" or "dkms".
WFB_MODULE_SOURCE = ADOS_RUN_DIR / "wfb-module-source"

# Last-locked WFB channel hint. Written by the ground-side receiver when
# a channel acquisition sweep locks onto the transmitter, so a restart
# can try that channel first instead of sweeping from scratch. This is a
# runtime HINT only: it lives on tmpfs (gone on reboot) and is NEVER the
# rendezvous home. The home channel is the operator's immutable
# ``video.wfb.channel`` in config.yaml; the agent must never auto-write
# that field. On a cold start with no established link the receiver homes
# on the configured channel and may consult this hint as a fast first
# guess, but always falls back to home. Single integer channel number as
# text; atomic tmp+replace write; missing/corrupt tolerated.
WFB_LOCKED_CHANNEL_HINT = ADOS_RUN_DIR / "wfb-locked-channel"

# USB gadget composer runtime artifacts
DNSMASQ_USB0_CONF = ADOS_RUN_DIR / "dnsmasq-usb0.conf"
DNSMASQ_USB0_PID = ADOS_RUN_DIR / "dnsmasq-usb0.pid"

# Live LCD shell state — current page id and modal stack identifiers,
# persisted across service restarts so the operator returns to the
# screen they last left after a reboot. Atomic-write JSON.
LCD_STATE_PATH = ADOS_RUN_DIR / "lcd-state.json"

# Remote page-set request file. Written by the REST surface
# (``POST /api/v1/display/page``) and consumed by the OLED service's
# navigator watcher. Atomic-write JSON; the watcher unlinks after
# applying so the same request is not reapplied on every tick.
LCD_PAGE_REQUEST_PATH = ADOS_RUN_DIR / "lcd-page-request.json"

# Local-video-tap stats published by the OLED service's video page on
# every tick. Consumed by the cloud heartbeat so the GCS Display
# sub-view can show whether the LCD is currently decoding video and
# at what FPS, without making the cloud subprocess reach into the
# OLED service's private state directly.
LCD_VIDEO_TAP_PATH = ADOS_RUN_DIR / "lcd-video-tap.json"

# PNG of the most recently rendered panel frame. The native display
# writer (``ados-display``) writes it after each render at ~1 Hz, so the
# REST snapshot endpoint (``GET /api/v1/display/snapshot``) can serve
# exactly what the LCD shows without re-reading the framebuffer or
# depending on PIL. Absent until the native writer has rendered a frame;
# the endpoint falls back to a direct framebuffer read in that window.
LCD_SNAPSHOT_PATH = ADOS_RUN_DIR / "lcd-snapshot.png"

# The in-process GStreamer air-side pipeline publishes its stats
# snapshot to this path at 1 Hz. Consumed by the REST surface (``GET
# /api/v1/video/air-pipeline``) and the cloud heartbeat enricher so the
# GCS can render encoder + pipeline-flavor pills without IPC into the
# video service. The file is absent when the legacy bash air pipeline
# owns the stream.
AIR_PIPELINE_STATS_PATH = ADOS_RUN_DIR / "air-pipeline.json"

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
DISPLAY_CONF_PATH = ADOS_ETC_DIR / "display.conf"
# Persistent marker written ONLY when a display has been provisioned or a
# physically-present panel was recognized. Services that drive a display
# (the on-board UI service, framebuffer-console detach) gate on this file
# so they skip cleanly on a board with no panel instead of running and
# failing. Removed on the no-display path.
DISPLAY_ENABLED_PATH = ADOS_ETC_DIR / "display.enabled"
# Probation marker for the apply-verify-auto-revert path. Written when a
# boot-critical SPI-LCD overlay is applied blind on a board that declares
# the panel but where it is not yet bound. Records the boot-config snapshot
# path so the boot-time probe can self-heal: confirm the panel after the
# overlay-applying reboot, or restore the snapshot when it never bound.
DISPLAY_PROBATION_PATH = ADOS_ETC_DIR / "display.probation"
ENV_FILE = ADOS_ETC_DIR / "env"
FIREWALL_RULES_PATH = ADOS_ETC_DIR / "firewall.rules"
AP_PASSPHRASE_PATH = ADOS_ETC_DIR / "ap-passphrase"

# Touchscreen calibration matrix saved by the LCD calibration wizard.
# JSON-serialized affine + metadata. Loaded by the touch input bridge
# at startup; absence triggers the wizard on first run when the touch
# chip is present.
TOUCH_CALIB_PATH = ADOS_ETC_DIR / "touch.calib"

# Secret material written by setup flows. Files under this directory should
# be created with owner-only permissions and must never be returned by APIs.
SECRETS_DIR = ADOS_ETC_DIR / "secrets"
CLOUDFLARE_TUNNEL_TOKEN_PATH = SECRETS_DIR / "cloudflare-tunnel-token"
# Same-origin setup token, used when security.setup_token_required=True.
# 0600 owner-only. CLI surfaces it in the status page.
SETUP_TOKEN_PATH = SECRETS_DIR / "setup-token"
# Self-hosted backend API key set during cloud-choice. 0600 owner-only.
SERVER_API_KEY_PATH = SECRETS_DIR / "server-api-key"

# Hostapd + dnsmasq config files (rendered on demand)
HOSTAPD_CONF_PATH = ADOS_ETC_DIR / "hostapd-gs.conf"
DNSMASQ_CONF_PATH = ADOS_ETC_DIR / "dnsmasq-gs.conf"

# Ground-station side-files (legacy + active migrations)
GS_UI_JSON = ADOS_ETC_DIR / "ground-station-ui.json"
GS_UPLINK_JSON = ADOS_ETC_DIR / "ground-station-uplink.json"
GS_INPUT_JSON = ADOS_ETC_DIR / "ground-station-input.json"
GS_MODEM_JSON = ADOS_ETC_DIR / "ground-station-modem.json"
GS_WIFI_CLIENT_JSON = ADOS_ETC_DIR / "ground-station-wifi-client.json"

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

# Persistent state files (setup wizard, hardware snapshot, etc.)
STATE_DIR = ADOS_VAR_DIR / "state"
SETUP_STATE_DIR = ADOS_VAR_DIR / "setup"
SETUP_STATE_PATH = SETUP_STATE_DIR / "state.json"

# Hardware-check snapshot. Written at first-boot, on operator
# Rescan, and on a successful TTL-bounded refresh inside the
# cached runner. Owned by the agent; readable by the GCS.
HARDWARE_STATE_PATH = SETUP_STATE_DIR / "hardware-state.json"

# Audit log
AUDIT_LOG_PATH = ADOS_VAR_DIR / "audit.jsonl"

# Plugins (installed third-party bundles, plugin data, plugin configs)
PLUGINS_INSTALL_DIR = ADOS_VAR_DIR / "plugins"
PLUGIN_DATA_DIR = ADOS_VAR_DIR / "plugin-data"
PLUGIN_LOG_DIR = Path("/var/log/ados/plugins")
PLUGIN_STATE_PATH = STATE_DIR / "plugin-state.json"

# Install-result record. Written atomically by the install pipeline at
# /var/lib/ados/install-result.json with the outcome of the last
# install/upgrade (status, version, profile, board, kernel release,
# radio-module source, failed and required-failure step lists). The
# heartbeat surfaces install health so the GCS can flag a degraded or
# failed install without an SSH session. Absent on older installs.
INSTALL_RESULT = Path("/var/lib/ados/install-result.json")
