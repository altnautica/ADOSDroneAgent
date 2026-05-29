"""Systemd unit generation for third-party plugins.

The architectural choice: each third-party plugin runs as a
generated systemd service ``ados-plugin-<id>.service`` inside the
shared ``ados-plugins.slice`` cgroup slice. Restart, watchdog, and
resource limits come from systemd; no manual cgroupv2 management.

Slice file: ``/etc/systemd/system/ados-plugins.slice``
    [Slice]
    CPUAccounting=yes
    MemoryAccounting=yes
    TasksAccounting=yes
    IOAccounting=yes

Per-plugin unit:

    [Unit]
    Description=ADOS plugin <plugin-id>
    After=ados-supervisor.service
    PartOf=ados-supervisor.service

    [Service]
    Slice=ados-plugins.slice
    Type=simple
    ExecStart=/opt/ados/venv/bin/ados-plugin-runner <plugin-id>
    Restart=on-failure
    RestartSec=2s
    StartLimitInterval=60s
    StartLimitBurst=5
    MemoryMax=<manifest.agent.resources.max_ram_mb>M
    CPUQuota=<manifest.agent.resources.max_cpu_percent>%
    TasksMax=<manifest.agent.resources.max_pids>
    StandardOutput=append:/var/log/ados/plugins/<id>.log
    StandardError=append:/var/log/ados/plugins/<id>.log
    User=ados
    Group=ados

    [Install]
    WantedBy=ados-supervisor.service

Built-in plugins (``isolation: inprocess``) skip this entirely; they
import into the supervisor's address space.
"""

from __future__ import annotations

from pathlib import Path

from ados.core.paths import (
    PLUGIN_LOG_DIR,
    PLUGIN_RUN_DIR,
    PLUGIN_UNIT_DIR,
    PLUGIN_UNIT_PREFIX,
)
from ados.plugins.manifest import PluginManifest

PLUGIN_RUNNER_BINARY = "/opt/ados/venv/bin/ados-plugin-runner"
PLUGIN_SLICE_NAME = "ados-plugins.slice"
PLUGIN_SLICE_PATH = PLUGIN_UNIT_DIR / PLUGIN_SLICE_NAME

PLUGIN_SLICE_CONTENT = """\
[Unit]
Description=ADOS plugin shared cgroup slice
Before=slices.target

[Slice]
CPUAccounting=yes
MemoryAccounting=yes
TasksAccounting=yes
IOAccounting=yes
"""


def slice_unit_content() -> str:
    return PLUGIN_SLICE_CONTENT


def unit_path_for(plugin_id: str) -> Path:
    safe = _sanitize_unit_name(plugin_id)
    # Read PLUGIN_UNIT_DIR from module globals each call so tests can rebind it.
    return globals()["PLUGIN_UNIT_DIR"] / f"{PLUGIN_UNIT_PREFIX}{safe}.service"


def unit_name_for(plugin_id: str) -> str:
    return f"{PLUGIN_UNIT_PREFIX}{_sanitize_unit_name(plugin_id)}.service"


def _sanitize_unit_name(plugin_id: str) -> str:
    """Convert reverse-DNS to a systemd-safe unit name.

    Plugin id ``com.example.thermal-lepton`` becomes
    ``com-example-thermal-lepton``. Periods are not allowed in unit
    file basenames before ``.service``; hyphens are.
    """
    return plugin_id.replace(".", "-")


def render_unit(manifest: PluginManifest, install_dir: Path) -> str:
    if manifest.agent is None:
        raise ValueError(
            f"plugin {manifest.id} has no agent half; no systemd unit needed"
        )
    if manifest.agent.isolation == "inprocess":
        raise ValueError(
            f"plugin {manifest.id} is inprocess; no systemd unit needed"
        )
    res = manifest.agent.resources
    # Reference the module global by name so tests can rebind it via
    # monkeypatch.setattr and the runtime resolves the current value.
    log_path = PLUGIN_LOG_DIR / f"{_sanitize_unit_name(manifest.id)}.log"
    # The ExecStart line is the only part that differs by agent.runtime.
    if manifest.agent.runtime == "rust":
        # Rust: exec the plugin's own binary directly with the plugin id as the
        # leading positional argument (non-secret; it is already in the install
        # path, and the SDK runner reads it positionally). The capability token
        # and agent id are delivered via the unit environment (ADOS_PLUGIN_TOKEN
        # / ADOS_PLUGIN_AGENT_ID) at cutover, never on the command line (a
        # /proc/<pid>/cmdline is world-readable).
        socket_path = PLUGIN_RUN_DIR / f"{manifest.id}.sock"
        exec_start = (
            f"{install_dir}/{manifest.id}/{manifest.agent.entrypoint} "
            f"{manifest.id} --socket {socket_path}"
        )
    else:
        # Python (default): the shared runner takes the plugin id and resolves
        # the manifest + entrypoint itself. Unchanged.
        exec_start = f"{PLUGIN_RUNNER_BINARY} {manifest.id}"
    return UNIT_TEMPLATE.format(
        plugin_id=manifest.id,
        slice_name=PLUGIN_SLICE_NAME,
        exec_start=exec_start,
        max_ram_mb=res.max_ram_mb,
        max_cpu_percent=res.max_cpu_percent,
        max_pids=res.max_pids,
        log_path=log_path,
    )


UNIT_TEMPLATE = """\
[Unit]
Description=ADOS plugin {plugin_id}
After=ados-supervisor.service
PartOf=ados-supervisor.service

[Service]
Slice={slice_name}
Type=simple
ExecStart={exec_start}
Restart=on-failure
RestartSec=2s
StartLimitInterval=60s
StartLimitBurst=5
MemoryMax={max_ram_mb}M
CPUQuota={max_cpu_percent}%
TasksMax={max_pids}
StandardOutput=append:{log_path}
StandardError=append:{log_path}
User=ados
Group=ados
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/ados/plugin-data /var/log/ados/plugins /run/ados/plugins
LockPersonality=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes

[Install]
WantedBy=ados-supervisor.service
"""
