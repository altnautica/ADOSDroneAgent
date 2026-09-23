"""Systemd unit generation for third-party plugins.

The architectural choice: each third-party plugin runs as a
generated systemd service ``ados-plugin-<id>.service`` inside the
shared ``ados-plugins.slice`` cgroup slice. Restart, watchdog, and
resource limits come from systemd; no manual cgroupv2 management.

Built-in plugins (``isolation: inprocess``) skip this entirely; they
import into the supervisor's address space.

**The unit is where three capabilities are enforced.** Opening
``/dev/i2c-1``, calling ``socket(AF_INET)``, or reading ``/srv`` are
direct syscalls inside the plugin's own process; there is no RPC the
host could gate, so the grant has to change the sandbox instead. The
``hardware.*`` grants become cgroup ``DeviceAllow=`` rules,
``network.outbound`` lifts the ``RestrictAddressFamilies`` /
``IPAddressDeny`` pair (only while the plugin host's nftables loopback
guard is loaded), and ``filesystem.host`` becomes the
``InaccessiblePaths`` / ``ReadWritePaths`` split. :func:`render_unit`
therefore takes the granted set, and the supervisor re-renders and
restarts on every grant and revoke — a grant that only took effect at
the next daemon restart was a security control reporting itself
applied while nothing had changed.

This module and ``ados-plugin-host``'s ``sandbox.rs`` +
``systemd.rs`` must render the same bytes for the same inputs: either
lifecycle path may be the one that wrote a given unit (this module
owns the local REST path, the Rust crate owns the cloud-relay path),
and a plugin must not get a different sandbox depending on which
surface the operator used. ``tests/test_plugins_systemd_sandbox.py``
asserts the two renderers agree line for line.
"""

from __future__ import annotations

import json
import re
import shlex
from collections.abc import Iterable
from pathlib import Path

from ados.core.paths import (
    PLUGIN_LOG_DIR,
    PLUGIN_LOOPBACK_GUARD_JSON,
    PLUGIN_RUN_DIR,
    PLUGIN_SOCKET_NAME,
    PLUGIN_UNGRANTABLE_CAPS_JSON,
    PLUGIN_UNIT_DIR,
    PLUGIN_UNIT_PREFIX,
)
from ados.plugins.manifest import PluginManifest
from ados.plugins.ready_check import PROBE_TIMEOUT_S, has_control_char

PLUGIN_RUNNER_BINARY = "/opt/ados/venv/bin/ados-plugin-runner"
PLUGIN_SLICE_NAME = "ados-plugins.slice"
PLUGIN_SLICE_PATH = PLUGIN_UNIT_DIR / PLUGIN_SLICE_NAME

#: The fixed hardening every plugin process runs under: the main unit, each
#: declared service, and each readiness probe. One list so the three cannot
#: drift apart.
HARDENING_DIRECTIVES = (
    "NoNewPrivileges=yes",
    "PrivateTmp=yes",
    "ProtectSystem=strict",
    "LockPersonality=yes",
    "RestrictRealtime=yes",
    "RestrictSUIDSGID=yes",
    "ProtectKernelTunables=yes",
    "ProtectKernelModules=yes",
    "ProtectControlGroups=yes",
    "ProtectProc=invisible",
    "RestrictNamespaces=yes",
    "SystemCallArchitectures=native",
)

# IOWeight=10 against the default 100 the flight units run at: systemd cannot
# bound the bandwidth of an ``append:`` log destination, so the only lever on a
# plugin that writes hard is I/O arbitration. A plugin loses every contended
# block against ados-mavlink and ados-video rather than delaying telemetry.
PLUGIN_SLICE_CONTENT = """\
[Unit]
Description=ADOS plugin shared cgroup slice
Before=slices.target

[Slice]
CPUAccounting=yes
MemoryAccounting=yes
TasksAccounting=yes
IOAccounting=yes
IOWeight=10
"""

# ---------------------------------------------------------------------------
# The capability-to-sandbox map. Byte-identical to
# `ados-plugin-host/src/sandbox.rs`; see that module for why each rule is the
# rule it is. Order is load-bearing: the rendered unit must be stable for a
# given grant set so re-rendering an unchanged set is a no-op.
# ---------------------------------------------------------------------------

#: Device capability -> the cgroup device-group rules it unlocks. The
#: right-hand side is a ``/proc/devices`` group name, not a path, so one rule
#: covers every minor the kernel enumerates.
DEVICE_CAP_RULES: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("hardware.uart", ("char-ttyUSB rw", "char-ttyACM rw", "char-tty rw")),
    ("hardware.i2c", ("char-i2c rw",)),
    ("hardware.spi", ("char-spidev rw",)),
    ("hardware.gpio", ("char-gpiochip rw",)),
    ("hardware.usb", ("char-usb_device rw",)),
    ("hardware.usb.uvc", ("char-video4linux rw",)),
    ("hardware.camera.csi", ("char-video4linux rw", "char-dri rw")),
)

NETWORK_OUTBOUND_CAP = "network.outbound"
FILESYSTEM_HOST_CAP = "filesystem.host"

#: Off limits granted or not: the HMAC issuer secret a plugin could mint
#: another plugin's token from, and the trusted-key store it could enrol its
#: own signer into. File modes already keep ``ados`` out of both.
ALWAYS_INACCESSIBLE = ("/etc/ados/secrets", "/etc/ados/plugin-keys")

#: The agent run directory, replaced by an empty read-only tmpfs in every
#: plugin's mount namespace. It holds every agent command socket, the plugin
#: host's control dir and every plugin's socket directory; each acts with the
#: agent's authority or another plugin's grants. The real gate is the
#: ``ados-operator`` socket group plus a peer-credential check on accept, and
#: this is the second line. Hiding the directory, not each socket, keeps a
#: socket a service re-creates after the plugin started hidden too.
HIDDEN_RUN_DIR = "/run/ados"

#: Sockets under :data:`HIDDEN_RUN_DIR` bound back into every plugin,
#: read-only: the log ingest sink, which only accepts log frames.
PLUGIN_REACHABLE_SOCKETS = ("/run/ados/logd.sock",)

#: Operator data roots reachable only with ``filesystem.host``.
HOST_DATA_ROOTS = ("/srv", "/mnt", "/media", "/boot")

#: The writable surface every plugin gets regardless of grants.
BASE_READ_WRITE_PATHS = (
    "/var/ados/plugin-data",
    "/var/log/ados/plugins",
)


def sandbox_enforced_caps() -> frozenset[str]:
    """Every capability whose enforcement mechanism is the generated unit."""
    return frozenset(
        [cap for cap, _ in DEVICE_CAP_RULES]
        + [NETWORK_OUTBOUND_CAP, FILESYSTEM_HOST_CAP]
    )


def plugin_loopback_guard_active() -> bool:
    """Whether the plugin host has loaded the loopback guard.

    Reads the verdict sidecar the plugin-host daemon writes at startup. Absent
    or unreadable reads as inactive: nothing may assume the rule is loaded.
    """
    try:
        data = json.loads(PLUGIN_LOOPBACK_GUARD_JSON.read_text())
    except (OSError, ValueError):
        return False
    return isinstance(data, dict) and data.get("active") is True


def host_ungrantable_caps() -> frozenset[str]:
    """Capabilities the running plugin host cannot back.

    Reads the list the plugin-host daemon writes at startup. Absent or
    unreadable reads as empty: the host then enforces its own refusal on the
    cloud path, and nothing here invents a list.
    """
    try:
        data = json.loads(PLUGIN_UNGRANTABLE_CAPS_JSON.read_text())
    except (OSError, ValueError):
        return frozenset()
    caps = data.get("caps") if isinstance(data, dict) else None
    if not isinstance(caps, list):
        return frozenset()
    return frozenset(c for c in caps if isinstance(c, str))


def sandbox_directives(
    granted: Iterable[str], loopback_guard_active: bool
) -> list[str]:
    """The ``[Service]`` lines expressing ``granted`` as a sandbox.

    ``loopback_guard_active`` is the plugin host's guard verdict; without it a
    ``network.outbound`` grant renders the no-grant socket policy.

    Deterministic for a given grant set, so an unchanged set re-renders to
    identical bytes and the supervisor can skip the restart.
    """
    granted_set = set(granted)
    lines: list[str] = []

    # ---- devices ----------------------------------------------------
    device_rules: list[str] = []
    for cap, rules in DEVICE_CAP_RULES:
        if cap in granted_set:
            device_rules.extend(rules)
    if not device_rules:
        # The strictest posture systemd offers: a private /dev holding only the
        # pseudo-devices, which also implies DevicePolicy=closed.
        lines.append("PrivateDevices=yes")
    else:
        lines.append("DevicePolicy=closed")
        # Dedupe keeping rule order (uvc and csi both want char-video4linux).
        seen: set[str] = set()
        for rule in device_rules:
            if rule not in seen:
                seen.add(rule)
                lines.append(f"DeviceAllow={rule}")

    # ---- sockets ----------------------------------------------------
    if NETWORK_OUTBOUND_CAP in granted_set and loopback_guard_active:
        # AF_NETLINK rides with the grant: a plugin that may reach the network
        # needs getifaddrs / DNS resolution to do it. The agent's own loopback
        # listeners stay closed through the plugin host's nftables guard.
        lines.append(
            "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK"
        )
    else:
        # AF_UNIX stays: the plugin's own host socket is a Unix socket.
        lines.append("RestrictAddressFamilies=AF_UNIX")
        lines.append("IPAddressDeny=any")

    # ---- filesystem -------------------------------------------------
    # The agent run dir goes first: an empty read-only tmpfs, with only the
    # plugin-reachable sockets bound back in.
    lines.append(f"TemporaryFileSystem={HIDDEN_RUN_DIR}:ro")
    lines.extend(f"BindReadOnlyPaths=-{p}" for p in PLUGIN_REACHABLE_SOCKETS)
    host_fs = FILESYSTEM_HOST_CAP in granted_set
    rw = list(BASE_READ_WRITE_PATHS)
    if host_fs:
        rw.extend(HOST_DATA_ROOTS)
    lines.append("ReadWritePaths=" + " ".join(rw))
    lines.append("ProtectHome=read-only" if host_fs else "ProtectHome=yes")
    inaccessible = [f"-{p}" for p in ALWAYS_INACCESSIBLE]
    if not host_fs:
        inaccessible.extend(f"-{p}" for p in HOST_DATA_ROOTS)
    lines.append("InaccessiblePaths=" + " ".join(inaccessible))

    return lines


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


#: An ``ExecStart`` word that needs no quoting: nothing systemd splits on,
#: unquotes, or expands (``%`` specifiers and ``$`` variables are excluded).
_EXEC_PLAIN_WORD = re.compile(r"^[A-Za-z0-9_./:=,@+-]+$")

#: Leading characters systemd reads as ``ExecStart`` prefixes rather than part
#: of the path. ``+`` and ``!`` lift the sandbox and run the command with full
#: privileges, so a plugin-authored command may not start with any of them.
_EXEC_PREFIX_CHARS = "-@:+!|"


def exec_start_value(command: str) -> str:
    """Render a plugin-authored ``command`` as one ``ExecStart=`` value.

    The command is split as argv (POSIX quoting, never a shell) and each word
    is re-emitted in systemd's own quoting, with ``%`` and ``$`` doubled so
    no specifier or variable expands. Refused with ``ValueError``: a control
    character (a newline would start a new directive, such as an
    ``ExecStartPre=+`` that runs as root), unbalanced quoting, an empty command,
    a first word carrying a systemd prefix character, and a lone ``;`` word
    (systemd's command separator).
    """
    if has_control_char(command):
        raise ValueError("service command must not contain control characters")
    try:
        argv = shlex.split(command)
    except ValueError as exc:
        raise ValueError(f"service command is not a valid argv: {exc}") from exc
    if not argv or not argv[0]:
        raise ValueError("service command must not be empty")
    if argv[0][0] in _EXEC_PREFIX_CHARS:
        raise ValueError(
            f"service command must not start with a systemd prefix ({argv[0][0]!r})"
        )
    if ";" in argv:
        raise ValueError("service command must not contain a lone ';' word")
    return " ".join(_exec_word(word) for word in argv)


def _exec_word(word: str) -> str:
    """One argv word in systemd ``ExecStart`` quoting."""
    if _EXEC_PLAIN_WORD.match(word):
        return word
    escaped = word.replace("\\", "\\\\").replace('"', '\\"')
    escaped = escaped.replace("%", "%%").replace("$", "$$")
    return f'"{escaped}"'



def render_unit(
    manifest: PluginManifest,
    install_dir: Path,
    granted: Iterable[str] = (),
) -> str:
    """Render a plugin's main runner unit.

    ``granted`` is the plugin's currently granted capability set. It only
    affects the sandbox block (:func:`sandbox_directives`), so passing the
    empty default renders the most restrictive unit — which is the correct
    posture at install time, before the operator has approved anything.
    """
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
    # The plugin's own socket directory is the one path under the hidden run
    # dir bound into the unit (read-only), so the plugin reaches its own host
    # socket and no other plugin's.
    socket_dir = PLUGIN_RUN_DIR / manifest.id
    socket_path = socket_dir / PLUGIN_SOCKET_NAME
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
    # rewritten on each start and on every rotation; the `-` prefix tolerates
    # its absence before the first mint without failing the unit. The runner
    # waits for the file rather than degrading, so the optional prefix cannot
    # produce a silently token-less plugin.
    token_env_file = PLUGIN_RUN_DIR / f"{manifest.id}.token.env"
    return UNIT_TEMPLATE.format(
        plugin_id=manifest.id,
        slice_name=PLUGIN_SLICE_NAME,
        socket_path=socket_path,
        token_env_file=token_env_file,
        socket_dir=socket_dir,
        exec_start=exec_start,
        max_ram_mb=res.max_ram_mb,
        max_cpu_percent=res.max_cpu_percent,
        max_pids=res.max_pids,
        log_path=log_path,
        hardening="\n".join(HARDENING_DIRECTIVES),
        sandbox="\n".join(sandbox_directives(granted, plugin_loopback_guard_active())),
    )


def render_service_unit(
    manifest: PluginManifest,
    service,
    install_dir: Path,
    granted: Iterable[str] = (),
) -> str:
    """Render a systemd unit for one plugin-declared extra service.

    The service runs its own ``ExecStart`` (``service.command``, validated and
    re-quoted by :func:`exec_start_value`) in the plugin's install directory,
    always in the shared plugin slice so resource accounting and the slice's
    I/O weight apply to it, with the same hardening flags as the main plugin
    unit. Resource limits come from the plugin's ``agent.resources`` so a
    declared service is bounded by the same envelope the operator approved at
    install. Raises ``ValueError`` for a command that cannot be rendered safely.

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
        slice_name=PLUGIN_SLICE_NAME,
        working_dir=plugin_dir,
        exec_start=exec_start_value(service.command),
        restart=service.restart,
        max_ram_mb=res.max_ram_mb,
        max_cpu_percent=res.max_cpu_percent,
        max_pids=res.max_pids,
        log_path=log_path,
        hardening="\n".join(HARDENING_DIRECTIVES),
        sandbox="\n".join(sandbox_directives(granted, plugin_loopback_guard_active())),
    )


def probe_command(
    manifest: PluginManifest,
    argv: tuple[str, ...],
    install_dir: Path,
    granted: Iterable[str] = (),
) -> list[str]:
    """The ``systemd-run`` argv that runs one readiness probe sandboxed.

    The probe is plugin-authored, so it gets exactly what the plugin's
    declared services get: the ``ados`` user, the shared hardening, the
    plugin's resource envelope and capability sandbox, the plugin slice, and
    the plugin's install dir as its working directory. systemd starts it as a
    transient unit, so it never runs inside the calling process, and
    ``--wait --pipe`` return its exit status and output. ``RuntimeMaxSec``
    bounds it in systemd as well as in the caller. ``argv`` is passed through
    as separate arguments; no shell ever sees it.
    """
    if manifest.agent is None:
        raise ValueError(f"plugin {manifest.id} has no agent half; nothing to probe")
    res = manifest.agent.resources
    properties = [
        *HARDENING_DIRECTIVES,
        f"MemoryMax={res.max_ram_mb}M",
        f"CPUQuota={res.max_cpu_percent}%",
        f"TasksMax={res.max_pids}",
        f"RuntimeMaxSec={PROBE_TIMEOUT_S}",
        *sandbox_directives(granted, plugin_loopback_guard_active()),
    ]
    return [
        "systemd-run",
        "--quiet",
        "--wait",
        "--pipe",
        "--collect",
        "--uid=ados",
        "--gid=ados",
        f"--slice={PLUGIN_SLICE_NAME}",
        f"--working-directory={install_dir / manifest.id}",
        *(f"--property={p}" for p in properties),
        "--",
        *argv,
    ]


UNIT_TEMPLATE = """\
[Unit]
Description=ADOS plugin {plugin_id}
After=ados-supervisor.service
PartOf=ados-supervisor.service
# No start rate limit: a plugin whose host socket is not up yet must keep
# retrying rather than land in a failed state an operator has to clear by hand.
StartLimitIntervalSec=0

[Service]
Slice={slice_name}
Type=simple
Environment=ADOS_PLUGIN_SOCKET={socket_path}
EnvironmentFile=-{token_env_file}
BindReadOnlyPaths={socket_dir}
ExecStart={exec_start}
Restart=on-failure
RestartSec=2s
MemoryMax={max_ram_mb}M
CPUQuota={max_cpu_percent}%
TasksMax={max_pids}
StandardOutput=append:{log_path}
StandardError=append:{log_path}
User=ados
Group=ados
{hardening}
# ---- capability sandbox (re-rendered on every grant/revoke) ----
{sandbox}

[Install]
WantedBy=ados-supervisor.service
"""


SERVICE_UNIT_TEMPLATE = """\
[Unit]
Description=ADOS plugin {plugin_id} service {service_name}
After=ados-supervisor.service
PartOf=ados-supervisor.service
StartLimitIntervalSec=0

[Service]
Slice={slice_name}
Type=simple
WorkingDirectory={working_dir}
ExecStart={exec_start}
Restart={restart}
RestartSec=2s
MemoryMax={max_ram_mb}M
CPUQuota={max_cpu_percent}%
TasksMax={max_pids}
StandardOutput=append:{log_path}
StandardError=append:{log_path}
User=ados
Group=ados
{hardening}
# ---- capability sandbox (re-rendered on every grant/revoke) ----
{sandbox}

[Install]
WantedBy=ados-supervisor.service
"""
