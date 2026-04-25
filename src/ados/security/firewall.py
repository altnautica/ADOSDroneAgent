"""Firewall rule generation and application using iptables."""

from __future__ import annotations

import platform
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

import structlog

from ados.core.paths import FIREWALL_RULES_PATH

log = structlog.get_logger(__name__)

DEFAULT_RULES_PATH = str(FIREWALL_RULES_PATH)


@dataclass
class FirewallConfig:
    """Configuration for firewall rule generation.

    All port lists use the standard ADOS defaults. Set optional flags
    to include extra services like MQTT or WireGuard.
    """

    ssh_port: int = 22
    api_port: int = 8080
    ws_port: int = 8765
    tcp_proxy_port: int = 5760
    udp_proxy_ports: list[int] = field(default_factory=lambda: [14550, 14551])
    allow_mqtt: bool = False
    mqtt_port: int = 8883
    allow_wireguard: bool = False
    wireguard_port: int = 51820
    extra_tcp_ports: list[int] = field(default_factory=list)
    extra_udp_ports: list[int] = field(default_factory=list)


def generate_firewall_rules(config: FirewallConfig | None = None) -> list[str]:
    """Generate iptables rules in whitelist mode (default DROP, explicit ALLOWs).

    Args:
        config: Firewall configuration. Uses defaults if None.

    Returns:
        List of iptables command strings.
    """
    if config is None:
        config = FirewallConfig()

    log.info("generating_firewall_rules", mqtt=config.allow_mqtt, wg=config.allow_wireguard)

    rules: list[str] = []

    # Flush existing rules
    rules.append("iptables -F INPUT")

    # Default policy: drop all incoming
    rules.append("iptables -P INPUT DROP")

    # Allow loopback
    rules.append("iptables -A INPUT -i lo -j ACCEPT")

    # Allow established and related connections
    rules.append("iptables -A INPUT -m state --state ESTABLISHED,RELATED -j ACCEPT")

    # SSH
    rules.append(f"iptables -A INPUT -p tcp --dport {config.ssh_port} -j ACCEPT")

    # Agent REST API
    rules.append(f"iptables -A INPUT -p tcp --dport {config.api_port} -j ACCEPT")

    # WebSocket relay
    rules.append(f"iptables -A INPUT -p tcp --dport {config.ws_port} -j ACCEPT")

    # MAVLink TCP proxy
    rules.append(f"iptables -A INPUT -p tcp --dport {config.tcp_proxy_port} -j ACCEPT")

    # MAVLink UDP proxy ports
    for port in config.udp_proxy_ports:
        rules.append(f"iptables -A INPUT -p udp --dport {port} -j ACCEPT")

    # Optional: MQTT (TLS)
    if config.allow_mqtt:
        rules.append(f"iptables -A INPUT -p tcp --dport {config.mqtt_port} -j ACCEPT")

    # Optional: WireGuard VPN
    if config.allow_wireguard:
        rules.append(f"iptables -A INPUT -p udp --dport {config.wireguard_port} -j ACCEPT")

    # Extra TCP ports
    for port in config.extra_tcp_ports:
        rules.append(f"iptables -A INPUT -p tcp --dport {port} -j ACCEPT")

    # Extra UDP ports
    for port in config.extra_udp_ports:
        rules.append(f"iptables -A INPUT -p udp --dport {port} -j ACCEPT")

    log.info("firewall_rules_generated", count=len(rules))
    return rules


def apply_firewall_rules(rules: list[str]) -> bool:
    """Apply iptables rules by executing each command.

    On macOS this gracefully skips execution since iptables is Linux-only.

    Args:
        rules: List of iptables command strings to execute.

    Returns:
        True if all rules applied successfully (or skipped on macOS), False on any failure.
    """
    if platform.system() != "Linux":
        log.info("firewall_apply_skipped", reason="not_linux", platform=platform.system())
        return True

    log.info("applying_firewall_rules", count=len(rules))
    for rule in rules:
        try:
            subprocess.run(
                rule.split(),
                check=True,
                capture_output=True,
                timeout=10,
            )
        except subprocess.CalledProcessError as exc:
            log.error("firewall_rule_failed", rule=rule, stderr=exc.stderr.decode())
            return False
        except FileNotFoundError:
            log.error("iptables_not_found", rule=rule)
            return False

    log.info("firewall_rules_applied", count=len(rules))
    return True


def save_firewall_rules(
    rules: list[str],
    path: str = DEFAULT_RULES_PATH,
) -> None:
    """Save generated firewall rules to a file.

    Creates parent directories if they don't exist. Logs a warning
    and returns gracefully if the directory is not writable.

    Args:
        rules: List of iptables command strings.
        path: File path to save rules to.
    """
    log.info("saving_firewall_rules", path=path, count=len(rules))

    filepath = Path(path)
    try:
        filepath.parent.mkdir(parents=True, exist_ok=True)
        filepath.write_text("\n".join(rules) + "\n")
        log.info("firewall_rules_saved", path=path)
    except PermissionError:
        log.warning("firewall_save_permission_denied", path=path)
    except OSError as exc:
        log.warning("firewall_save_failed", path=path, error=str(exc))
