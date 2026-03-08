"""WireGuard VPN tunnel management via wg-quick."""

from __future__ import annotations

import platform
import subprocess

from ados.core.config import WireguardConfig
from ados.core.logging import get_logger

log = get_logger("wireguard")

_CMD_TIMEOUT = 15


class WireguardManager:
    """Manages a WireGuard tunnel using wg-quick and wg CLI tools.

    On macOS, all operations log a warning and return gracefully
    since WireGuard management requires Linux.
    """

    def __init__(self, config: WireguardConfig) -> None:
        self._config = config
        self._interface = "ados"

    def _is_linux(self) -> bool:
        return platform.system() == "Linux"

    def _run_cmd(self, args: list[str]) -> tuple[bool, str]:
        """Run a shell command and return (success, output)."""
        if not self._is_linux():
            log.warning("wireguard_not_linux", platform=platform.system())
            return False, "WireGuard management requires Linux"

        try:
            result = subprocess.run(
                args,
                capture_output=True,
                text=True,
                timeout=_CMD_TIMEOUT,
            )
            if result.returncode == 0:
                return True, result.stdout.strip()
            return False, result.stderr.strip()
        except FileNotFoundError:
            log.error("wireguard_binary_not_found", cmd=args[0])
            return False, f"{args[0]} not found"
        except subprocess.TimeoutExpired:
            log.error("wireguard_command_timeout", cmd=" ".join(args))
            return False, "Command timed out"

    def start_tunnel(self) -> bool:
        """Start the WireGuard tunnel using wg-quick."""
        if not self._config.enabled:
            log.info("wireguard_disabled")
            return False

        log.info("starting_wireguard", interface=self._interface)
        ok, output = self._run_cmd(["wg-quick", "up", self._interface])
        if ok:
            log.info("wireguard_started", interface=self._interface)
        else:
            log.error("wireguard_start_failed", output=output)
        return ok

    def stop_tunnel(self) -> bool:
        """Stop the WireGuard tunnel using wg-quick."""
        log.info("stopping_wireguard", interface=self._interface)
        ok, output = self._run_cmd(["wg-quick", "down", self._interface])
        if ok:
            log.info("wireguard_stopped", interface=self._interface)
        else:
            log.error("wireguard_stop_failed", output=output)
        return ok

    def is_active(self) -> bool:
        """Check if the WireGuard interface is active."""
        ok, _ = self._run_cmd(["wg", "show", self._interface])
        return ok

    def get_status(self) -> dict:
        """Get WireGuard interface info, peer, and transfer bytes.

        Returns a dict with interface details, or an empty dict
        if the tunnel is not active or not on Linux.
        """
        if not self._is_linux():
            return {"active": False, "reason": "not_linux"}

        ok, output = self._run_cmd(["wg", "show", self._interface])
        if not ok:
            return {"active": False}

        info: dict = {"active": True, "interface": self._interface, "raw": output}

        # Parse key fields from wg show output
        for line in output.splitlines():
            stripped = line.strip()
            if stripped.startswith("public key:"):
                info["public_key"] = stripped.split(":", 1)[1].strip()
            elif stripped.startswith("transfer:"):
                info["transfer"] = stripped.split(":", 1)[1].strip()
            elif stripped.startswith("latest handshake:"):
                info["latest_handshake"] = stripped.split(":", 1)[1].strip()
            elif stripped.startswith("endpoint:"):
                info["endpoint"] = stripped.split(":", 1)[1].strip()

        return info

    def generate_keypair(self) -> tuple[str, str]:
        """Generate a WireGuard keypair using wg CLI tools.

        Returns (private_key, public_key) as base64-encoded strings.
        Returns empty strings on failure or non-Linux.
        """
        ok, private_key = self._run_cmd(["wg", "genkey"])
        if not ok:
            return "", ""

        # Pipe private key into wg pubkey
        if not self._is_linux():
            return "", ""

        try:
            result = subprocess.run(
                ["wg", "pubkey"],
                input=private_key,
                capture_output=True,
                text=True,
                timeout=_CMD_TIMEOUT,
            )
            if result.returncode == 0:
                public_key = result.stdout.strip()
                log.info("wireguard_keypair_generated")
                return private_key, public_key
        except (FileNotFoundError, subprocess.TimeoutExpired):
            log.error("wireguard_pubkey_failed")

        return "", ""
