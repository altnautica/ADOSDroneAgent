"""ADOS ROS 2 environment manager.

Manages the Docker container lifecycle for the opt-in ROS 2 Jazzy
environment. Handles image pull/load, compose rendering, start/stop,
health checks, and status queries.

DEC-111: ROS is opt-in. This service only starts when ros.enabled=true
in the agent config AND the board profile has ros.supported=true.
"""

from __future__ import annotations

import asyncio
import json
import logging
import shutil
import subprocess
import sys
from enum import Enum
from pathlib import Path
from typing import Any

from jinja2 import Template

from ados.core.config import ADOSConfig, load_config

log = logging.getLogger("ados.services.ros_manager")

# Container and compose paths
COMPOSE_TEMPLATE = Path(__file__).parent.parent.parent.parent / "docker" / "ros" / "docker-compose.yml.j2"
COMPOSE_OUTPUT = Path("/var/ados/ros/docker-compose.yml")
WORKSPACE_DEFAULT = Path("/opt/ados/ros-ws")


class RosState(str, Enum):
    """ROS environment lifecycle states."""

    NOT_INITIALIZED = "not_initialized"
    INITIALIZING = "initializing"
    READY = "ready"
    RUNNING = "running"
    ERROR = "error"
    STOPPED = "stopped"


class RosManager:
    """Manages the ROS 2 Docker container lifecycle."""

    def __init__(self, config: ADOSConfig) -> None:
        self._config = config
        self._state = RosState.NOT_INITIALIZED
        self._error: str | None = None
        self._container_id: str | None = None

    @property
    def state(self) -> RosState:
        return self._state

    @property
    def error(self) -> str | None:
        return self._error

    # ── Preflight checks ─────────────────────────────────────────────

    def check_prerequisites(self) -> list[str]:
        """Check that all prerequisites are met for ROS init.

        Returns a list of blocking issues (empty = all clear).
        """
        issues: list[str] = []

        # Docker must be installed
        if not shutil.which("docker"):
            issues.append("Docker is not installed. Run install.sh --with-ros")

        # Docker daemon must be running
        result = subprocess.run(
            ["docker", "info"],
            capture_output=True,
            timeout=10,
        )
        if result.returncode != 0:
            issues.append("Docker daemon is not running")

        # MAVLink socket must exist
        if not Path("/run/ados/mavlink.sock").exists():
            issues.append("MAVLink IPC socket not found. Is ados-mavlink running?")

        # Disk space check (need at least 2 GB for image + workspace)
        try:
            import os
            stat = os.statvfs("/")
            free_gb = (stat.f_bavail * stat.f_frsize) / (1024 ** 3)
            if free_gb < 2.0:
                issues.append(f"Insufficient disk space: {free_gb:.1f} GB free, need 2+ GB")
        except OSError:
            pass

        return issues

    # ── Image management ─────────────────────────────────────────────

    async def pull_image(self) -> bool:
        """Pull the ROS image from registry."""
        ros = self._config.ros
        image = f"{ros.image_name}:{ros.image_tag}"
        log.info("pulling ROS image", extra={"image": image})

        proc = await asyncio.create_subprocess_exec(
            "docker", "pull", image,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            self._error = f"Failed to pull image: {stderr.decode().strip()}"
            log.error(self._error)
            return False
        return True

    async def load_offline_image(self) -> bool:
        """Load the ROS image from a pre-staged tarball."""
        ros = self._config.ros
        tarball = Path(ros.offline_image_path)

        if not tarball.exists():
            self._error = f"Offline image not found at {tarball}"
            log.error(self._error)
            return False

        log.info("loading offline ROS image", extra={"path": str(tarball)})

        # Support both .tar and .tar.zst
        if tarball.suffix == ".zst" or tarball.name.endswith(".tar.zst"):
            cmd = f"zstd -d -c {tarball} | docker load"
            proc = await asyncio.create_subprocess_shell(
                cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
        else:
            proc = await asyncio.create_subprocess_exec(
                "docker", "load", "-i", str(tarball),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )

        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            self._error = f"Failed to load image: {stderr.decode().strip()}"
            log.error(self._error)
            return False
        return True

    def image_exists(self) -> bool:
        """Check if the ROS image is already available locally."""
        ros = self._config.ros
        image = f"{ros.image_name}:{ros.image_tag}"
        result = subprocess.run(
            ["docker", "image", "inspect", image],
            capture_output=True,
            timeout=10,
        )
        return result.returncode == 0

    # ── Compose rendering ────────────────────────────────────────────

    def render_compose(self) -> Path:
        """Render docker-compose.yml from the Jinja2 template."""
        ros = self._config.ros
        agent = self._config.agent

        # Find template (dev: relative to source, prod: /opt/ados/data/)
        template_path = COMPOSE_TEMPLATE
        if not template_path.exists():
            template_path = Path("/opt/ados/data/ros/docker-compose.yml.j2")
        if not template_path.exists():
            # Fallback for installed package
            import importlib.resources
            ref = importlib.resources.files("ados").joinpath(
                "../../docker/ros/docker-compose.yml.j2"
            )
            template_path = Path(str(ref))

        template_text = template_path.read_text(encoding="utf-8")
        tmpl = Template(template_text)

        rmw_map = {
            "zenoh": "rmw_zenoh_cpp",
            "cyclonedds": "rmw_cyclonedds_cpp",
        }

        workspace_path = Path(ros.workspace_path)
        workspace_path.mkdir(parents=True, exist_ok=True)

        rendered = tmpl.render(
            image_name=ros.image_name,
            image_tag=ros.image_tag,
            domain_id=ros.domain_id,
            rmw_implementation=rmw_map.get(ros.middleware, "rmw_zenoh_cpp"),
            device_id=agent.device_id,
            profile=ros.profile,
            foxglove_port=ros.foxglove_port,
            workspace_path=str(workspace_path),
            memory_limit_mb=ros.memory_limit_mb,
            cpu_limit=ros.cpu_limit,
        )

        COMPOSE_OUTPUT.parent.mkdir(parents=True, exist_ok=True)
        COMPOSE_OUTPUT.write_text(rendered, encoding="utf-8")
        log.info("rendered docker-compose.yml", extra={"path": str(COMPOSE_OUTPUT)})
        return COMPOSE_OUTPUT

    # ── Container lifecycle ──────────────────────────────────────────

    async def initialize(self) -> bool:
        """Full initialization: preflight, image, compose, start.

        Yields progress events as a generator for SSE streaming.
        """
        self._state = RosState.INITIALIZING
        self._error = None

        # Step 1: Preflight
        issues = self.check_prerequisites()
        if issues:
            self._error = "; ".join(issues)
            self._state = RosState.ERROR
            return False

        # Step 2: Image
        if not self.image_exists():
            offline_path = Path(self._config.ros.offline_image_path)
            if offline_path.exists():
                success = await self.load_offline_image()
            else:
                success = await self.pull_image()
            if not success:
                self._state = RosState.ERROR
                return False

        # Step 3: Compose
        try:
            self.render_compose()
        except Exception as exc:
            self._error = f"Failed to render compose: {exc}"
            self._state = RosState.ERROR
            return False

        # Step 4: Start
        success = await self.start()
        if not success:
            self._state = RosState.ERROR
            return False

        self._state = RosState.RUNNING
        return True

    async def start(self) -> bool:
        """Start the ROS container via docker compose."""
        compose_path = COMPOSE_OUTPUT
        if not compose_path.exists():
            self.render_compose()

        log.info("starting ROS container")
        proc = await asyncio.create_subprocess_exec(
            "docker", "compose", "-f", str(compose_path), "up", "-d",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            self._error = f"Failed to start container: {stderr.decode().strip()}"
            log.error(self._error)
            return False

        # Wait for healthcheck to pass
        for _ in range(30):
            if await self._is_healthy():
                self._state = RosState.RUNNING
                log.info("ROS container is healthy")
                return True
            await asyncio.sleep(2)

        self._error = "Container started but healthcheck timed out after 60s"
        log.warning(self._error)
        # Still consider it running, just warn
        self._state = RosState.RUNNING
        return True

    async def stop(self) -> bool:
        """Stop the ROS container."""
        compose_path = COMPOSE_OUTPUT
        if not compose_path.exists():
            self._state = RosState.STOPPED
            return True

        log.info("stopping ROS container")
        proc = await asyncio.create_subprocess_exec(
            "docker", "compose", "-f", str(compose_path), "down",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        await proc.communicate()
        self._state = RosState.STOPPED
        return True

    async def _is_healthy(self) -> bool:
        """Check if the container is healthy via docker inspect."""
        result = subprocess.run(
            ["docker", "inspect", "--format", "{{.State.Health.Status}}", "ados-ros"],
            capture_output=True,
            timeout=5,
        )
        return result.returncode == 0 and result.stdout.decode().strip() == "healthy"

    # ── Status queries ───────────────────────────────────────────────

    def get_status(self) -> dict[str, Any]:
        """Get current ROS environment status."""
        status: dict[str, Any] = {
            "state": self._state.value,
            "error": self._error,
            "distro": "jazzy",
            "middleware": self._config.ros.middleware,
            "profile": self._config.ros.profile,
            "foxglove_port": self._config.ros.foxglove_port,
            "foxglove_url": None,
            "container_id": None,
            "uptime_s": None,
            "nodes_count": 0,
            "topics_count": 0,
        }

        if self._state != RosState.RUNNING:
            return status

        # Get container info
        try:
            result = subprocess.run(
                ["docker", "inspect", "ados-ros"],
                capture_output=True,
                timeout=5,
            )
            if result.returncode == 0:
                info = json.loads(result.stdout)[0]
                status["container_id"] = info.get("Id", "")[:12]
                # Calculate uptime
                started = info.get("State", {}).get("StartedAt", "")
                if started:
                    from datetime import datetime, timezone
                    try:
                        start_dt = datetime.fromisoformat(started.replace("Z", "+00:00"))
                        now = datetime.now(timezone.utc)
                        status["uptime_s"] = int((now - start_dt).total_seconds())
                    except (ValueError, TypeError):
                        pass
        except (subprocess.TimeoutExpired, json.JSONDecodeError):
            pass

        status["foxglove_url"] = f"ws://localhost:{self._config.ros.foxglove_port}"

        # Get node and topic counts
        try:
            nodes = self._exec_ros2_cmd("node", "list")
            if nodes:
                status["nodes_count"] = len([n for n in nodes.splitlines() if n.strip()])
        except Exception:
            pass

        try:
            topics = self._exec_ros2_cmd("topic", "list")
            if topics:
                status["topics_count"] = len([t for t in topics.splitlines() if t.strip()])
        except Exception:
            pass

        return status

    def get_nodes(self) -> list[dict[str, Any]]:
        """Get list of running ROS 2 nodes with basic info."""
        if self._state != RosState.RUNNING:
            return []

        try:
            output = self._exec_ros2_cmd("node", "list")
            if not output:
                return []

            nodes = []
            for line in output.splitlines():
                name = line.strip()
                if not name:
                    continue
                node: dict[str, Any] = {"name": name, "package": "", "pid": None}

                # Get node info for publishers/subscribers
                try:
                    info = self._exec_ros2_cmd("node", "info", name)
                    if info:
                        pubs = []
                        subs = []
                        section = None
                        for info_line in info.splitlines():
                            stripped = info_line.strip()
                            if "Publishers:" in stripped:
                                section = "pub"
                            elif "Subscribers:" in stripped:
                                section = "sub"
                            elif "Service Servers:" in stripped:
                                section = None
                            elif section == "pub" and stripped.startswith("/"):
                                topic = stripped.split(":")[0].strip()
                                pubs.append(topic)
                            elif section == "sub" and stripped.startswith("/"):
                                topic = stripped.split(":")[0].strip()
                                subs.append(topic)
                        node["publishes"] = pubs
                        node["subscribes"] = subs
                except Exception:
                    node["publishes"] = []
                    node["subscribes"] = []

                nodes.append(node)
            return nodes
        except Exception as exc:
            log.warning("failed to list nodes: %s", exc)
            return []

    def get_topics(self) -> list[dict[str, Any]]:
        """Get list of active ROS 2 topics with types."""
        if self._state != RosState.RUNNING:
            return []

        try:
            output = self._exec_ros2_cmd("topic", "list", "-t")
            if not output:
                return []

            topics = []
            for line in output.splitlines():
                stripped = line.strip()
                if not stripped:
                    continue
                # Format: /topic_name [type/Name]
                parts = stripped.split(" ", 1)
                name = parts[0]
                msg_type = parts[1].strip("[]") if len(parts) > 1 else ""
                topics.append({
                    "name": name,
                    "type": msg_type,
                    "publishers": 0,
                    "subscribers": 0,
                    "rate_hz": None,
                })
            return topics
        except Exception as exc:
            log.warning("failed to list topics: %s", exc)
            return []

    def _exec_ros2_cmd(self, *args: str) -> str | None:
        """Execute a ros2 CLI command inside the container."""
        try:
            result = subprocess.run(
                ["docker", "exec", "ados-ros", "ros2", *args],
                capture_output=True,
                timeout=10,
            )
            if result.returncode == 0:
                return result.stdout.decode("utf-8", errors="replace")
        except subprocess.TimeoutExpired:
            log.warning("ros2 command timed out: %s", " ".join(args))
        return None


# ── Singleton ────────────────────────────────────────────────────────

_manager: RosManager | None = None


def get_ros_manager(config: ADOSConfig | None = None) -> RosManager:
    """Get or create the singleton ROS manager."""
    global _manager
    if _manager is None:
        if config is None:
            config = load_config()
        _manager = RosManager(config)
    return _manager


# ── Main entry point (for systemd ExecStart) ────────────────────────

async def _run() -> None:
    """Main loop: start the ROS container and keep the service alive."""
    config = load_config()

    if not config.ros.enabled:
        log.info("ROS integration is disabled (ros.enabled=false)")
        # Keep the service alive but idle so supervisor doesn't restart-loop
        while True:
            await asyncio.sleep(3600)

    manager = get_ros_manager(config)

    # Initialize and start
    success = await manager.initialize()
    if not success:
        log.error("ROS initialization failed: %s", manager.error)
        sys.exit(1)

    log.info("ROS environment running. Waiting for shutdown signal.")

    # Keep alive until killed by systemd
    try:
        while True:
            await asyncio.sleep(30)
            # Periodic health check
            if not await manager._is_healthy():
                log.warning("ROS container health check failed, restarting...")
                await manager.stop()
                await manager.start()
    except asyncio.CancelledError:
        log.info("Shutdown requested")
        await manager.stop()


def _stop() -> None:
    """Stop handler for systemd ExecStop."""
    import asyncio as _aio
    config = load_config()
    manager = get_ros_manager(config)
    _aio.run(manager.stop())


if __name__ == "__main__":
    if "--stop" in sys.argv:
        _stop()
    else:
        asyncio.run(_run())
