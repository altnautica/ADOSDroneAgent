# SPDX-License-Identifier: GPL-3.0-only
# Copyright (c) 2026 Altnautica
"""ROS 2 workspace manager for the ADOS Drone Agent.

Handles package scaffolding from Jinja2 templates, workspace listing,
build triggering, and metadata queries. All operations run inside the
Docker container via ``docker exec``.

Template resolution order:
    1. /opt/ados/data/ros/templates/ (production install)
    2. data/ros/templates/ relative to the repo root (dev)
"""

from __future__ import annotations

import asyncio
import shutil
import subprocess
from pathlib import Path
from typing import Any, AsyncGenerator

import structlog
from jinja2 import Environment, FileSystemLoader

log = structlog.get_logger("ados.services.ros_workspace")

# Template search paths (production first, dev fallback)
_TEMPLATE_DIRS = [
    Path("/opt/ados/data/ros/templates"),
    Path(__file__).parent.parent.parent.parent / "data" / "ros" / "templates",
]

CONTAINER_NAME = "ados-ros"
CONTAINER_WS = "/root/ados_ws"


def _template_root() -> Path:
    """Return the first existing template directory."""
    for d in _TEMPLATE_DIRS:
        if d.is_dir():
            return d
    raise FileNotFoundError(
        "No ROS template directory found. "
        "Expected /opt/ados/data/ros/templates/ or data/ros/templates/"
    )


def _exec_in_container(
    *args: str, timeout: int = 30
) -> subprocess.CompletedProcess[bytes]:
    """Run a command inside the ados-ros container."""
    return subprocess.run(
        ["docker", "exec", CONTAINER_NAME, *args],
        capture_output=True,
        timeout=timeout,
    )


def scaffold_package(
    name: str,
    template: str = "rclpy_basic",
    workspace_path: str = "/opt/ados/ros-ws",
) -> Path:
    """Create a new ROS 2 package from a Jinja2 template.

    Args:
        name: Package name (must be a valid Python identifier).
        template: Template name (rclpy_basic, rclpy_planner, rclpy_perception).
        workspace_path: Host path to the workspace bind-mounted into the container.

    Returns:
        Path to the created package directory on the host.

    Raises:
        ValueError: If the template does not exist or the name is invalid.
        FileExistsError: If the package already exists.
    """
    if not name.isidentifier():
        raise ValueError(f"Package name must be a valid Python identifier: {name}")

    root = _template_root()
    tmpl_dir = root / template
    if not tmpl_dir.is_dir():
        available = [d.name for d in root.iterdir() if d.is_dir()]
        raise ValueError(
            f"Unknown template '{template}'. Available: {', '.join(available)}"
        )

    pkg_dir = Path(workspace_path) / "src" / name
    if pkg_dir.exists():
        raise FileExistsError(f"Package already exists at {pkg_dir}")

    env = Environment(
        loader=FileSystemLoader(str(tmpl_dir)),
        keep_trailing_newline=True,
    )

    context = {
        "package_name": name,
        "node_name": name,
        "maintainer": "Altnautica",
        "maintainer_email": "team@altnautica.com",
    }

    pkg_dir.mkdir(parents=True, exist_ok=True)

    # Create the Python package directory
    py_pkg = pkg_dir / name
    py_pkg.mkdir(exist_ok=True)
    (py_pkg / "__init__.py").write_text("", encoding="utf-8")

    # Resource marker
    resource_dir = pkg_dir / "resource"
    resource_dir.mkdir(exist_ok=True)
    (resource_dir / name).write_text("", encoding="utf-8")

    # Launch directory
    launch_dir = pkg_dir / "launch"
    launch_dir.mkdir(exist_ok=True)

    # Render templates
    for tmpl_file in tmpl_dir.iterdir():
        if not tmpl_file.name.endswith(".j2"):
            continue

        tmpl = env.get_template(tmpl_file.name)
        rendered = tmpl.render(**context)
        out_name = tmpl_file.name.removesuffix(".j2")

        # Route files to the right subdirectory
        if out_name == "node.py":
            dest = py_pkg / f"{name}_node.py"
        elif out_name == "launch.py":
            dest = launch_dir / f"{name}.launch.py"
        else:
            dest = pkg_dir / out_name

        dest.write_text(rendered, encoding="utf-8")
        log.debug("rendered template", file=str(dest))

    log.info("scaffolded ROS package", package=name, template=template, path=str(pkg_dir))
    return pkg_dir


def list_packages(workspace_path: str = "/opt/ados/ros-ws") -> list[dict[str, Any]]:
    """List packages in the workspace src/ directory.

    Queries the container for packages via ``docker exec``. Falls back to
    reading the host filesystem if the container is not running.

    Returns:
        List of dicts with ``name`` and ``path`` keys.
    """
    packages: list[dict[str, Any]] = []

    # Try container first
    try:
        result = _exec_in_container(
            "bash", "-c",
            f"ls -1 {CONTAINER_WS}/src/ 2>/dev/null || true",
            timeout=10,
        )
        if result.returncode == 0:
            for line in result.stdout.decode("utf-8", errors="replace").splitlines():
                name = line.strip()
                if name:
                    packages.append({
                        "name": name,
                        "path": f"{CONTAINER_WS}/src/{name}",
                    })
            return packages
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    # Fallback: read host filesystem
    src_dir = Path(workspace_path) / "src"
    if src_dir.is_dir():
        for child in sorted(src_dir.iterdir()):
            if child.is_dir() and (child / "package.xml").exists():
                packages.append({
                    "name": child.name,
                    "path": str(child),
                })

    return packages


def get_info(workspace_path: str = "/opt/ados/ros-ws") -> dict[str, Any]:
    """Return workspace metadata.

    Returns:
        Dict with ``packages`` (count), ``last_build`` (ISO timestamp or None),
        ``disk_used_mb`` (float), and ``workspace_path``.
    """
    info: dict[str, Any] = {
        "workspace_path": workspace_path,
        "packages": 0,
        "last_build": None,
        "disk_used_mb": 0.0,
    }

    pkgs = list_packages(workspace_path)
    info["packages"] = len(pkgs)

    ws_host = Path(workspace_path)
    install_dir = ws_host / "install"
    if install_dir.is_dir():
        # Get modification time of the install dir as a proxy for last build
        import datetime
        mtime = install_dir.stat().st_mtime
        info["last_build"] = datetime.datetime.fromtimestamp(
            mtime, tz=datetime.timezone.utc
        ).isoformat()

    # Calculate disk usage
    if ws_host.is_dir():
        try:
            result = subprocess.run(
                ["du", "-sm", str(ws_host)],
                capture_output=True,
                timeout=10,
            )
            if result.returncode == 0:
                parts = result.stdout.decode().strip().split("\t")
                if parts:
                    info["disk_used_mb"] = float(parts[0])
        except (subprocess.TimeoutExpired, ValueError):
            pass

    return info


async def build(
    workspace_path: str = "/opt/ados/ros-ws",
    package: str | None = None,
) -> AsyncGenerator[str, None]:
    """Trigger a colcon build inside the container, yielding output lines.

    Args:
        workspace_path: Not used directly (container always builds at CONTAINER_WS).
        package: Optional specific package to build. If None, builds all.

    Yields:
        Lines of colcon build output as they arrive.
    """
    cmd_parts = [
        "bash", "-c",
        "source /opt/ros/jazzy/setup.bash && "
        "source /opt/ados/ros-packages/install/setup.bash && "
        f"cd {CONTAINER_WS} && "
        "colcon build --merge-install"
    ]

    if package:
        cmd_parts[-1] += f" --packages-select {package}"

    cmd_parts[-1] += " 2>&1"

    proc = await asyncio.create_subprocess_exec(
        "docker", "exec", CONTAINER_NAME, *cmd_parts,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )

    if proc.stdout is None:
        yield "Error: failed to capture build output"
        return

    while True:
        line = await proc.stdout.readline()
        if not line:
            break
        yield line.decode("utf-8", errors="replace").rstrip("\n")

    await proc.wait()
    rc = proc.returncode or 0
    if rc == 0:
        yield "[ados] Build completed successfully."
    else:
        yield f"[ados] Build failed with exit code {rc}."
