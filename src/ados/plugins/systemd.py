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
    Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/<plugin-id>.sock
    EnvironmentFile=-/run/ados/plugins/<plugin-id>.token.env
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


def service_unit_name_for(plugin_id: str, service_name: str) -> str:
    """Unit name for a plugin-declared extra service.

    Distinct from :func:`unit_name_for` (the plugin's main runner unit)
    by the trailing ``-<service>`` segment, so the declared services
    never collide with the main unit or with each other.
    """
    return (
        f"{PLUGIN_UNIT_PREFIX}{_sanitize_unit_name(plugin_id)}"
        f"-{_sanitize_unit_name(service_name)}.service"
    )


def service_unit_path_for(plugin_id: str, service_name: str) -> Path:
    return globals()["PLUGIN_UNIT_DIR"] / service_unit_name_for(
        plugin_id, service_name
    )


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
    socket_path = PLUGIN_RUN_DIR / f"{manifest.id}.sock"
    # The ExecStart line is the only part that differs by agent.runtime.
    if manifest.agent.runtime == "rust":
        # Rust: exec the plugin's own binary directly with the plugin id as the
        # leading positional argument (non-secret; it is already in the install
        # path, and the SDK runner reads it positionally). The capability token
        # and socket path are delivered via the unit environment, never on the
        # command line (a /proc/<pid>/cmdline is world-readable).
        exec_start = (
            f"{install_dir}/{manifest.id}/{manifest.agent.entrypoint} "
            f"{manifest.id} --socket {socket_path}"
        )
    else:
        # Python (default): the shared runner takes the plugin id and resolves
        # the manifest + entrypoint itself. Unchanged.
        exec_start = f"{PLUGIN_RUNNER_BINARY} {manifest.id}"
    # Token delivery: a 0600 EnvironmentFile carries ADOS_PLUGIN_TOKEN (and
    # ADOS_PLUGIN_SOCKET) into the runner, which reads both from its
    # environment (the click options default to os.environ.get). The file is
    # rewritten with a fresh token on each start; the `-` prefix tolerates its
    # absence before the first mint without failing the unit. The socket path
    # is also a static Environment line as a fallback for the env-file race.
    token_env_file = PLUGIN_RUN_DIR / f"{manifest.id}.token.env"
    return UNIT_TEMPLATE.format(
        plugin_id=manifest.id,
        slice_name=PLUGIN_SLICE_NAME,
        socket_path=socket_path,
        token_env_file=token_env_file,
        exec_start=exec_start,
        max_ram_mb=res.max_ram_mb,
        max_cpu_percent=res.max_cpu_percent,
        max_pids=res.max_pids,
        log_path=log_path,
    )


def render_service_unit(
    manifest: PluginManifest,
    service,
    install_dir: Path,
) -> str:
    """Render a systemd unit for one plugin-declared extra service.

    The service runs its own ``ExecStart`` (``service.command``) in the
    plugin's install directory, under the spec's slice (defaulting to
    the shared plugin slice so resource accounting stays grouped), with
    the same hardening flags as the main plugin unit. Resource limits
    come from the plugin's ``agent.resources`` so a declared service is
    bounded by the same envelope the operator approved at install.

    ``service`` is a ``ServiceSpec`` from
    ``manifest.agent.contributes.services``.
    """
    if manifest.agent is None:
        raise ValueError(
            f"plugin {manifest.id} has no agent half; no service unit needed"
        )
    res = manifest.agent.resources
    plugin_dir = install_dir / manifest.id
    safe_service = _sanitize_unit_name(service.name)
    log_path = (
        PLUGIN_LOG_DIR
        / f"{_sanitize_unit_name(manifest.id)}-{safe_service}.log"
    )
    return SERVICE_UNIT_TEMPLATE.format(
        plugin_id=manifest.id,
        service_name=service.name,
        slice_name=service.slice or PLUGIN_SLICE_NAME,
        working_dir=plugin_dir,
        exec_start=service.command,
        restart=service.restart,
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
Environment=ADOS_PLUGIN_SOCKET={socket_path}
EnvironmentFile=-{token_env_file}
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


SERVICE_UNIT_TEMPLATE = """\
[Unit]
Description=ADOS plugin {plugin_id} service {service_name}
After=ados-supervisor.service
PartOf=ados-supervisor.service

[Service]
Slice={slice_name}
Type=simple
WorkingDirectory={working_dir}
ExecStart={exec_start}
Restart={restart}
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
