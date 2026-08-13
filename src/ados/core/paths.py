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

Base-directory resolution
-------------------------
The three top-level bases resolve through :func:`_run_base`, :func:`_etc_base`,
and :func:`_var_base`. On Linux with no environment override they are the fixed
FHS paths above, unchanged. Two things shift them:

* an explicit ``ADOS_RUN_DIR`` / ``ADOS_ETC_DIR`` / ``ADOS_VAR_DIR`` override
  (the same variables the Rust services read), and
* macOS, where the agent runs rootless under ``$HOME/.ados`` (the layout the
  installer's ``macos.rs`` writes), so the CLI (``ados logs`` / ``status`` /
  ``pair`` / ``unpair``) resolves the per-user paths without the operator
  exporting anything.

``PAIRING_JSON`` and ``INSTALL_RESULT`` additionally honour their own
``ADOS_PAIRING_JSON`` / ``ADOS_INSTALL_RESULT`` overrides (the exact variables
the workstation daemons are launched with).
"""

import os
import platform
from pathlib import Path

_IS_MACOS = platform.system() == "Darwin"


def _ados_home() -> Path:
    """The rootless per-user install root used on macOS (``$HOME/.ados``).

    Mirrors the installer's ``macos.rs`` layout. Honours ``ADOS_HOME`` when the
    workstation daemons exported it; otherwise ``$HOME/.ados``. Unused on Linux,
    where the FHS bases below are the default.
    """
    override = os.environ.get("ADOS_HOME")
    if override:
        return Path(override)
    home = os.environ.get("HOME") or str(Path.home())
    return Path(home) / ".ados"


def _run_base() -> Path:
    """Runtime dir: ``ADOS_RUN_DIR`` override, else ``~/.ados/run`` on macOS, else
    the Linux FHS ``/run/ados`` (Linux default unchanged)."""
    override = os.environ.get("ADOS_RUN_DIR")
    if override:
        return Path(override)
    return _ados_home() / "run" if _IS_MACOS else Path("/run/ados")


def _etc_base() -> Path:
    """Config/identity dir: ``ADOS_ETC_DIR`` override, else ``~/.ados`` on macOS
    (the installer writes ``config.yaml`` / ``pairing.json`` / ``device-id`` /
    ``profile.conf`` directly under ``~/.ados``), else ``/etc/ados`` (Linux
    default unchanged)."""
    override = os.environ.get("ADOS_ETC_DIR")
    if override:
        return Path(override)
    return _ados_home() if _IS_MACOS else Path("/etc/ados")


def _var_base() -> Path:
    """Agent-owned data dir: ``ADOS_VAR_DIR`` override, else ``~/.ados/var`` on
    macOS, else the Linux FHS ``/var/ados`` (Linux default unchanged)."""
    override = os.environ.get("ADOS_VAR_DIR")
    if override:
        return Path(override)
    return _ados_home() / "var" if _IS_MACOS else Path("/var/ados")


def _lib_base() -> Path:
    """Install-orchestration dir: ``ADOS_LIB_DIR`` override, else ``~/.ados`` on
    macOS (where ``installer/macos.rs`` records the install result beside
    ``config.yaml``), else the Linux FHS ``/var/lib/ados`` (Linux default
    unchanged, and the same literal ``ados-installer``'s ``env.rs`` STATE_DIR
    carries)."""
    override = os.environ.get("ADOS_LIB_DIR")
    if override:
        return Path(override)
    return _ados_home() if _IS_MACOS else Path("/var/lib/ados")


# ---------------------------------------------------------------------------
# Runtime directory: /run/ados/
# Sockets, pid files, ephemeral state. Wiped on reboot by tmpfs.
# ---------------------------------------------------------------------------

ADOS_RUN_DIR = _run_base()

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

# Operator WiFi-join/forget command socket served by the native ``ados-net``
# uplink daemon. The REST `/network/client/*` handlers forward to this when the
# native daemon owns the uplink, so they never drive `nmcli` on `wlan0`
# in-process and race the daemon's WiFi manager for the radio.
WIFI_CMD_SOCK = ADOS_RUN_DIR / "wifi-cmd.sock"

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

# The detected HAL board dict, persisted once at status time so a separate
# on-box reader (the native control surface) can serve the full board block
# without an in-process HAL-detect port of its own.
BOARD_JSON = ADOS_RUN_DIR / "board.json"
MESH_STATE_JSON = ADOS_RUN_DIR / "mesh-state.json"
WFB_RELAY_JSON = ADOS_RUN_DIR / "wfb-relay.json"
WFB_RECEIVER_JSON = ADOS_RUN_DIR / "wfb-receiver.json"
# Cross-process field-pairing event journal. The in-process pairing bus lives in
# the API process; the native control surface that serves the mesh event stream
# is a separate process and cannot reach that bus, so the pairing manager mirrors
# each pair event here as one newline-delimited JSON object. The native handler
# tails this file alongside the mesh-event journal. Same envelope shape as the
# mesh journal (`{"bus","kind","timestamp_ms","payload"}`), append-only,
# best-effort; bounded by the tmpfs wipe on reboot.
PAIR_EVENTS_JSONL = ADOS_RUN_DIR / "pair-events.jsonl"

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

# Local-bind to cloud-relay failover state. Written by the always-on
# auto-pair supervisor (a separate process from the API) when a fresh
# rig keeps failing to bind locally and falls back to the cloud relay.
# Read by GET /api/wfb/pair/failover-status. Single ``{"state": ...}``
# JSON object, atomic write, mode 0o644; default ``local`` when absent.
WFB_FAILOVER_STATE_JSON = ADOS_RUN_DIR / "wfb_failover.json"

# Sentinel files
UPLINK_ACTIVE_FLAG = ADOS_RUN_DIR / "uplink-active"

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

# PNG of the most recently rendered panel frame. The native display
# writer (``ados-display``) writes it after each render at ~1 Hz, so the
# REST snapshot endpoint (``GET /api/v1/display/snapshot``) can serve
# exactly what the LCD shows without re-reading the framebuffer or
# depending on PIL. Absent until the native writer has rendered a frame;
# the endpoint falls back to a direct framebuffer read in that window.
LCD_SNAPSHOT_PATH = ADOS_RUN_DIR / "lcd-snapshot.png"

# ---------------------------------------------------------------------------
# Config directory: /etc/ados/
# Persistent operator-owned configuration. Written by the installer,
# the pairing flow, and the REST API.
# ---------------------------------------------------------------------------

ADOS_ETC_DIR = _etc_base()

# Top-level config + identity
CONFIG_YAML = ADOS_ETC_DIR / "config.yaml"
DEVICE_ID_PATH = ADOS_ETC_DIR / "device-id"
# The pairing document. Honours ``ADOS_PAIRING_JSON`` (the exact variable the
# workstation daemons are launched with) first, so the CLI reads the same file
# the control surface writes; else it lands beside the other identity files.
PAIRING_JSON = (
    Path(os.environ["ADOS_PAIRING_JSON"])
    if os.environ.get("ADOS_PAIRING_JSON")
    else ADOS_ETC_DIR / "pairing.json"
)
PROFILE_CONF = ADOS_ETC_DIR / "profile.conf"
BOARD_OVERRIDE_PATH = ADOS_ETC_DIR / "board_override"
DISPLAY_CONF_PATH = ADOS_ETC_DIR / "display.conf"
# Marker mirroring `radio.crsf.enabled`: present ⇔ the CRSF RC lane is opted
# in. The ados-crsf systemd unit gates on it (ConditionPathExists) so a node
# that never enables the lane skips the unit cleanly. Reconciled by the config
# persist path and by the installer, never hand-managed.
CRSF_ENABLED_PATH = ADOS_ETC_DIR / "crsf-enabled"
# Marker mirroring `radio.tunnel.enabled`: present ⇔ the config-over-radio
# channel is opted in. The ados-tunnel-config systemd unit gates on it
# (ConditionPathExists) so a node that never enables the channel skips the unit
# cleanly. Reconciled by the config persist path and by the installer, never
# hand-managed.
TUNNEL_ENABLED_PATH = ADOS_ETC_DIR / "tunnel-enabled"
# Marker mirroring `network.hotspot.enabled`: present ⇔ the ground-station
# setup AP is opted in. The ados-dnsmasq-gs systemd unit gates on it
# (ConditionPathExists) because hostapd's unit idles-in-place rather than
# exiting when the operator has not opted in, so it stays `active` and cannot
# itself gate the DHCP/DNS unit. Reconciled by the config persist path and by
# the installer, never hand-managed.
HOTSPOT_ENABLED_PATH = ADOS_ETC_DIR / "hotspot-enabled"
FIREWALL_RULES_PATH = ADOS_ETC_DIR / "firewall.rules"
AP_PASSPHRASE_PATH = ADOS_ETC_DIR / "ap-passphrase"

# Touchscreen calibration matrix saved by the LCD calibration wizard.
# JSON-serialized affine + metadata. Loaded by the touch input bridge
# at startup; absence triggers the wizard on first run when the touch
# chip is present.
TOUCH_CALIB_PATH = ADOS_ETC_DIR / "touch.calib"

# udev rule that carries the LIBINPUT_CALIBRATION_MATRIX for an HDMI display's
# standalone SPI resistive-touch layer (XPT2046/ADS7846). Written by the
# display-overlay installer from the declared touch bounds, and regenerated by
# the calibration wizard when it refits the touch on the rig, so cage/libinput
# maps the resistive contact onto the HDMI output. Not under /etc/ados because
# udev only reads rules from its own rules.d directories.
HDMI_TOUCH_UDEV_RULE_PATH = Path("/etc/udev/rules.d/99-ados-hdmi-touch.rules")

# Secret material written by setup flows. Files under this directory should
# be created with owner-only permissions and must never be returned by APIs.
SECRETS_DIR = ADOS_ETC_DIR / "secrets"
CLOUDFLARE_TUNNEL_TOKEN_PATH = SECRETS_DIR / "cloudflare-tunnel-token"
# Self-hosted backend API key set during cloud-choice. 0600 owner-only.
SERVER_API_KEY_PATH = SECRETS_DIR / "server-api-key"

# Hostapd + dnsmasq config files (rendered on demand)
HOSTAPD_CONF_PATH = ADOS_ETC_DIR / "hostapd-gs.conf"
DNSMASQ_CONF_PATH = ADOS_ETC_DIR / "dnsmasq-gs.conf"

# Ground-station side-files (legacy + active migrations)
GS_UI_JSON = ADOS_ETC_DIR / "ground-station-ui.json"
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

# ---------------------------------------------------------------------------
# Data directory: /var/ados/
# Agent-owned persistent data. Recordings, OTA state, logs, downloads.
# ---------------------------------------------------------------------------

ADOS_VAR_DIR = _var_base()

# Recordings + media
RECORDINGS_DIR = ADOS_VAR_DIR / "recordings"

# Flight logs
FLIGHT_LOGS_DIR = ADOS_VAR_DIR / "logs/flights"

# Persistent state files (setup wizard, hardware snapshot, etc.)
STATE_DIR = ADOS_VAR_DIR / "state"
SETUP_STATE_DIR = ADOS_VAR_DIR / "setup"
SETUP_STATE_PATH = SETUP_STATE_DIR / "state.json"

# Hardware-check snapshot. Written at first-boot, on operator
# Rescan, and on a successful TTL-bounded refresh inside the
# cached runner. Owned by the agent; readable by the GCS.
HARDWARE_STATE_PATH = SETUP_STATE_DIR / "hardware-state.json"

# Plugins (installed third-party bundles, plugin data, plugin configs)
PLUGINS_INSTALL_DIR = ADOS_VAR_DIR / "plugins"
PLUGIN_DATA_DIR = ADOS_VAR_DIR / "plugin-data"
ADOS_LOG_DIR = Path("/var/log/ados")
PLUGIN_LOG_DIR = ADOS_LOG_DIR / "plugins"
PLUGIN_STATE_PATH = STATE_DIR / "plugin-state.json"

# Install-result record. Written atomically by the install pipeline with the
# outcome of the last install/upgrade (status, version, profile, board, kernel
# release, radio-module source, failed and required-failure step lists). The
# heartbeat surfaces install health so the GCS can flag a degraded or failed
# install without an SSH session. Absent on older installs. Honours
# ``ADOS_INSTALL_RESULT`` (the variable the workstation daemons + Linux env file
# carry); default is the Linux FHS path, or ``~/.ados/install-result.json`` on
# macOS (where the installer records it).
# The operator audit trail: an append-only, newline-delimited record of the
# decisions that persist (a regulatory posture, a plugin capability grant, a pair
# transition, a sandbox rule enforced). Written by `ados.core.audit`; budgeted,
# trimmed and rendered by the supervisor's disk janitor + `ados diag`, which
# constrain only that it is append-only and newline-delimited.
AUDIT_LOG = ADOS_VAR_DIR / "audit.jsonl"

INSTALL_RESULT = (
    Path(os.environ["ADOS_INSTALL_RESULT"])
    if os.environ.get("ADOS_INSTALL_RESULT")
    else _lib_base() / "install-result.json"
)

# Per-step ``<name>.done`` markers the install pipeline drops so an interrupted
# run can resume. `ados install --status` reads them to show done vs missing.
# Aligned with ``crates/ados-installer/src/env.rs``'s ``CHECKPOINT_DIR``; the
# Rust side keeps the Linux literal because the installer only ever runs on a
# target, while this constant also has to resolve on a macOS workstation.
INSTALL_CHECKPOINT_DIR = (
    Path(os.environ["ADOS_INSTALL_CHECKPOINT_DIR"])
    if os.environ.get("ADOS_INSTALL_CHECKPOINT_DIR")
    else _lib_base() / "install-checkpoints"
)


# ---------------------------------------------------------------------------
# Factory reset — the canonical credential set
# ---------------------------------------------------------------------------

# Standing credentials a factory reset MUST destroy.
#
# Factory reset is what an operator runs before handing a unit to somebody
# else, so anything left behind is access the previous holder keeps. Each of
# these is a credential in its own right, not a cache of one:
#
#   pairing.json        the API key the data plane accepts
#   dashboard-pin.json  mints dashboard sessions
#   mcp-token.json      a scoped bearer token the auth edge accepts in place
#                       of the API key
#   secrets/            cloudflare tunnel token, setup token, server API key
#   ap-passphrase       the access point's WPA2 key
#   wfb/                the radio keypair, which is the fleet's join gate
#   certs/              TLS material
#
# Identity and configuration, also destroyed:
#
#   device-id      the unit's identity. A factory reset means the box comes
#                  back indistinguishable from a freshly flashed one, so the
#                  identity goes too. It reappears in the GCS as a new device
#                  and has to be added again — that is the intended reading of
#                  "factory reset", and the safest default before handing the
#                  hardware to somebody else.
#   config.yaml    operator configuration; regenerates from defaults.
#   /var/log/ados  operational history from the previous holder.
#
# Deliberately NOT reset:
#
#   profile.conf   holds `profile`, `channel` and `version` — what this
#                  hardware IS, not who it belongs to. It is 0644 and carries
#                  no secret. Removing it strips the profile marker, and a
#                  later bare upgrade then reprofiles the box, which has
#                  already cost one rig a full reflash.
#
# Both the shell script and the API path consume this list, and a test asserts
# they agree: the three implementations diverged in the first place because
# each carried its own copy, and the shell script was already erasing identity
# while the API path preserved it.

DASHBOARD_PIN_PATH = ADOS_ETC_DIR / "dashboard-pin.json"
MCP_TOKEN_PATH = ADOS_ETC_DIR / "mcp-token.json"
WFB_KEY_DIR = ADOS_ETC_DIR / "wfb"
CERTS_DIR = ADOS_ETC_DIR / "certs"
SETUP_COMPLETE_PATH = _lib_base() / "setup-complete"

#: Files a factory reset unlinks. Credentials first, so an interrupted run has
#: already destroyed what grants access rather than only what identifies the box.
FACTORY_RESET_FILES: tuple[Path, ...] = (
    PAIRING_JSON,
    DASHBOARD_PIN_PATH,
    MCP_TOKEN_PATH,
    AP_PASSPHRASE_PATH,
    SETUP_COMPLETE_PATH,
    # Identity and configuration, after the credentials.
    DEVICE_ID_PATH,
    CONFIG_YAML,
)

#: Directories a factory reset empties.
FACTORY_RESET_DIRS: tuple[Path, ...] = (
    SECRETS_DIR,
    WFB_KEY_DIR,
    CERTS_DIR,
    ADOS_LOG_DIR,
)
