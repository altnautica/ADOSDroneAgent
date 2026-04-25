# SPDX-License-Identifier: GPL-3.0-only
# Copyright (c) 2026 Altnautica
"""`ados ros ...` subcommands for the ROS 2 integration.

Provides CLI access to the ROS 2 environment managed by
``ados.services.ros_manager``. Covers status, initialization,
package scaffolding, building, topic echo, bag recording,
diagnostics, and interactive shell.

Registered under the main ``cli`` group in ``ados.cli.main`` via
``cli.add_command(ros)``.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path

import click
import httpx

from ados.core.paths import PAIRING_JSON, ROS_RECORDINGS_DIR

API_BASE = "http://localhost:8080"
PAIRING_STATE_PATH = PAIRING_JSON
CONTAINER_NAME = "ados-ros"


# ---------------------------------------------------------------------------
# Auth helpers (same pattern as gs.py)
# ---------------------------------------------------------------------------


def _load_api_key() -> str | None:
    """Read /etc/ados/pairing.json for the X-ADOS-Key value."""
    try:
        if PAIRING_STATE_PATH.exists():
            with open(PAIRING_STATE_PATH) as f:
                data = json.load(f)
            key = data.get("api_key")
            return str(key) if key else None
    except (OSError, ValueError, json.JSONDecodeError):
        pass
    return None


def _headers() -> dict[str, str]:
    key = _load_api_key()
    return {"X-ADOS-Key": key} if key else {}


def _api_get(path: str) -> dict | None:
    """GET request to the local agent API."""
    try:
        resp = httpx.get(f"{API_BASE}{path}", headers=_headers(), timeout=10.0)
        resp.raise_for_status()
        return resp.json()
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPStatusError as e:
        click.echo(f"Error: API returned {e.response.status_code}", err=True)
        return None
    except httpx.HTTPError as e:
        click.echo(f"Error: {e}", err=True)
        return None


def _api_post(path: str, body: dict | None = None, timeout: float = 30.0) -> dict | None:
    """POST request to the local agent API."""
    try:
        resp = httpx.post(
            f"{API_BASE}{path}",
            headers=_headers(),
            json=body or {},
            timeout=timeout,
        )
        resp.raise_for_status()
        return resp.json()
    except httpx.ConnectError:
        click.echo(
            "Error: Agent is not running. Start with 'ados start' or 'ados demo'.",
            err=True,
        )
        sys.exit(1)
    except httpx.HTTPStatusError as e:
        click.echo(f"Error: API returned {e.response.status_code}", err=True)
        try:
            detail = e.response.json()
            click.echo(json.dumps(detail, indent=2), err=True)
        except Exception:
            pass
        return None
    except httpx.HTTPError as e:
        click.echo(f"Error: {e}", err=True)
        return None


def _docker_exec(*args: str, timeout: int = 30) -> subprocess.CompletedProcess[bytes]:
    """Run a command inside the ados-ros container."""
    return subprocess.run(
        ["docker", "exec", CONTAINER_NAME, *args],
        capture_output=True,
        timeout=timeout,
    )


def _docker_exec_interactive(*args: str) -> None:
    """Run an interactive command inside the ados-ros container."""
    subprocess.run(
        ["docker", "exec", "-it", CONTAINER_NAME, *args],
    )


def _ros2_cmd(*args: str, timeout: int = 15) -> str | None:
    """Execute a ros2 CLI command in the container, return stdout."""
    try:
        result = _docker_exec("ros2", *args, timeout=timeout)
        if result.returncode == 0:
            return result.stdout.decode("utf-8", errors="replace")
        stderr = result.stderr.decode("utf-8", errors="replace").strip()
        if stderr:
            click.echo(f"ros2 error: {stderr}", err=True)
    except subprocess.TimeoutExpired:
        click.echo(f"Timeout: ros2 {' '.join(args)}", err=True)
    except FileNotFoundError:
        click.echo("Error: docker not found on PATH", err=True)
    return None


# ---------------------------------------------------------------------------
# Command group
# ---------------------------------------------------------------------------


@click.group("ros")
def ros() -> None:
    """ROS 2 environment management.

    Manage the opt-in ROS 2 Jazzy Docker environment, including
    initialization, package scaffolding, building, topic inspection,
    bag recording, and diagnostics.
    """
    pass


# ---------------------------------------------------------------------------
# ados ros status
# ---------------------------------------------------------------------------


@ros.command()
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON.")
def status(as_json: bool) -> None:
    """Show ROS 2 environment status."""
    data = _api_get("/api/ros/status")
    if not data:
        return

    if as_json:
        click.echo(json.dumps(data, indent=2))
        return

    click.echo(f"State:       {data.get('state', '?')}")
    click.echo(f"Distro:      {data.get('distro', '?')}")
    click.echo(f"Middleware:   {data.get('middleware', '?')}")
    click.echo(f"Profile:     {data.get('profile', '?')}")
    click.echo(f"Nodes:       {data.get('nodes_count', 0)}")
    click.echo(f"Topics:      {data.get('topics_count', 0)}")

    container_id = data.get("container_id")
    if container_id:
        click.echo(f"Container:   {container_id}")

    uptime = data.get("uptime_s")
    if uptime is not None:
        minutes, secs = divmod(int(uptime), 60)
        hours, minutes = divmod(minutes, 60)
        click.echo(f"Uptime:      {hours}h {minutes}m {secs}s")

    foxglove = data.get("foxglove_url")
    if foxglove:
        click.echo(f"Foxglove:    {foxglove}")

    error = data.get("error")
    if error:
        click.echo(f"Error:       {error}")


# ---------------------------------------------------------------------------
# ados ros init
# ---------------------------------------------------------------------------


@ros.command("init")
@click.option(
    "--profile",
    type=click.Choice(["minimal", "vio", "mapping"]),
    default="minimal",
    help="ROS profile to initialize.",
)
@click.option(
    "--middleware",
    type=click.Choice(["zenoh", "cyclonedds"]),
    default="zenoh",
    help="RMW implementation to use.",
)
def init_ros(profile: str, middleware: str) -> None:
    """Initialize the ROS 2 environment.

    Pulls the Docker image (or loads from offline tarball), renders
    docker-compose.yml, and starts the container with the selected
    profile and middleware.
    """
    click.echo(f"Initializing ROS 2 (profile={profile}, middleware={middleware})...")

    data = _api_post("/api/ros/init", {
        "profile": profile,
        "middleware": middleware,
    }, timeout=300.0)

    if data:
        if data.get("success"):
            click.echo("ROS 2 environment initialized and running.")
        else:
            click.echo(f"Initialization failed: {data.get('error', 'unknown')}", err=True)
    else:
        click.echo("Failed to initialize. Check agent logs.", err=True)


# ---------------------------------------------------------------------------
# ados ros create-node
# ---------------------------------------------------------------------------


@ros.command("create-node")
@click.argument("name")
@click.option(
    "--template",
    type=click.Choice(["basic", "planner", "perception"]),
    default="basic",
    help="Package template to use.",
)
def create_node(name: str, template: str) -> None:
    """Scaffold a new ROS 2 package from a template.

    Creates a complete ament_python package in the workspace src/
    directory with a node, launch file, and build configuration.
    """
    template_name = f"rclpy_{template}"

    try:
        from ados.services.ros_workspace import scaffold_package
        pkg_path = scaffold_package(name, template=template_name)
        click.echo(f"Package created: {pkg_path}")
        click.echo(f"Build with: ados ros build --package {name}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)
    except FileExistsError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)
    except FileNotFoundError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


# ---------------------------------------------------------------------------
# ados ros launch
# ---------------------------------------------------------------------------


@ros.command("launch")
@click.argument("package")
@click.argument("executable")
def launch_node(package: str, executable: str) -> None:
    """Launch a ROS 2 node inside the container.

    Runs the specified executable from the given package using
    ros2 run. Output is streamed to the terminal.
    """
    click.echo(f"Launching {package}/{executable}...")

    data = _api_post("/api/ros/launch", {
        "package": package,
        "executable": executable,
    })

    if data:
        if data.get("success"):
            click.echo(f"Node launched: {package}/{executable}")
        else:
            click.echo(f"Launch failed: {data.get('error', 'unknown')}", err=True)
    else:
        click.echo("Failed to launch node. Check agent logs.", err=True)


# ---------------------------------------------------------------------------
# ados ros build
# ---------------------------------------------------------------------------


@ros.command("build")
@click.option("--package", "-p", default=None, help="Build a specific package only.")
def build(package: str | None) -> None:
    """Trigger a colcon build in the workspace.

    Runs colcon build inside the container and streams the output.
    Use --package to build only a specific package.
    """
    async def _build() -> None:
        from ados.services.ros_workspace import build as ws_build

        click.echo("Starting build...")
        async for line in ws_build(package=package):
            click.echo(line)

    try:
        asyncio.run(_build())
    except KeyboardInterrupt:
        click.echo("\nBuild interrupted.")


# ---------------------------------------------------------------------------
# ados ros topic echo
# ---------------------------------------------------------------------------


@ros.command("topic")
@click.argument("topic_name")
@click.option("--count", "-n", default=0, help="Number of messages to show (0 = unlimited).")
def topic_echo(topic_name: str, count: int) -> None:
    """Echo messages from a ROS 2 topic.

    Streams topic messages from inside the container to the terminal.
    Press Ctrl+C to stop.
    """
    cmd = ["docker", "exec", CONTAINER_NAME, "bash", "-c",
           f"source /opt/ros/jazzy/setup.bash && "
           f"source /opt/ados/ros-packages/install/setup.bash && "
           f"ros2 topic echo {topic_name}"]

    if count > 0:
        cmd[-1] += f" --once" if count == 1 else ""

    try:
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        messages = 0
        while True:
            if proc.stdout is None:
                break
            line = proc.stdout.readline()
            if not line:
                break
            click.echo(line.decode("utf-8", errors="replace"), nl=False)
            if line.strip() == b"---":
                messages += 1
                if count > 0 and messages >= count:
                    proc.terminate()
                    break
    except KeyboardInterrupt:
        if proc:
            proc.terminate()
    except FileNotFoundError:
        click.echo("Error: docker not found on PATH", err=True)


# ---------------------------------------------------------------------------
# ados ros bag record
# ---------------------------------------------------------------------------


@ros.command("record")
@click.option("--topics", "-t", multiple=True, help="Topics to record (repeatable).")
@click.option("--duration", "-d", default=0, type=int, help="Duration in seconds (0 = until stopped).")
@click.option("--max-size", default=500, type=int, help="Max file size in MB.")
def bag_record(topics: tuple[str, ...], duration: int, max_size: int) -> None:
    """Start recording topics to an MCAP bag file.

    Records ROS 2 topics into MCAP format inside the container.
    Files are stored at /var/ados/ros/recordings/.
    """
    async def _record() -> None:
        from ados.services.ros_recording import get_recording_manager

        mgr = get_recording_manager()
        topic_list = list(topics) if topics else None
        rec_id = await mgr.start(
            topics=topic_list,
            max_size_mb=max_size,
            max_duration_s=duration,
        )
        click.echo(f"Recording started: {rec_id}")
        if topic_list:
            click.echo(f"Topics: {', '.join(topic_list)}")
        else:
            click.echo("Topics: all")
        click.echo("Stop with: ados ros bag-stop " + rec_id)

    try:
        asyncio.run(_record())
    except KeyboardInterrupt:
        click.echo("\nRecording continues in background.")


# ---------------------------------------------------------------------------
# ados ros bag-stop
# ---------------------------------------------------------------------------


@ros.command("bag-stop")
@click.argument("recording_id")
def bag_stop(recording_id: str) -> None:
    """Stop a running bag recording."""
    async def _stop() -> None:
        from ados.services.ros_recording import get_recording_manager

        mgr = get_recording_manager()
        ok = await mgr.stop(recording_id)
        if ok:
            click.echo(f"Recording stopped: {recording_id}")
        else:
            click.echo(f"Recording not found: {recording_id}", err=True)

    asyncio.run(_stop())


# ---------------------------------------------------------------------------
# ados ros bag-list
# ---------------------------------------------------------------------------


@ros.command("bag-list")
@click.option("--json", "as_json", is_flag=True, help="Output raw JSON.")
def bag_list(as_json: bool) -> None:
    """List bag recordings."""
    from ados.services.ros_recording import get_recording_manager

    mgr = get_recording_manager()
    recordings = mgr.list_recordings()

    if as_json:
        click.echo(json.dumps(recordings, indent=2))
        return

    if not recordings:
        click.echo("No recordings found.")
        return

    for rec in recordings:
        active_tag = " [RECORDING]" if rec.get("active") else ""
        click.echo(
            f"  {rec['name']}  {rec.get('size_mb', 0):.1f} MB  "
            f"{rec.get('file_count', 0)} files  {rec.get('created', '?')}{active_tag}"
        )


# ---------------------------------------------------------------------------
# ados ros bag play
# ---------------------------------------------------------------------------


@ros.command("bag-play")
@click.argument("file_path")
def bag_play(file_path: str) -> None:
    """Play back a recorded MCAP bag file.

    Replays the bag file inside the container. Press Ctrl+C to stop.
    """
    # Determine the container path
    container_path = file_path
    _ros_recordings_str = str(ROS_RECORDINGS_DIR)
    if not file_path.startswith(_ros_recordings_str):
        container_path = f"{_ros_recordings_str}/{file_path}"

    cmd = [
        "docker", "exec", CONTAINER_NAME, "bash", "-c",
        f"source /opt/ros/jazzy/setup.bash && "
        f"source /opt/ados/ros-packages/install/setup.bash && "
        f"ros2 bag play {container_path}",
    ]

    try:
        subprocess.run(cmd)
    except KeyboardInterrupt:
        click.echo("\nPlayback stopped.")
    except FileNotFoundError:
        click.echo("Error: docker not found on PATH", err=True)


# ---------------------------------------------------------------------------
# ados ros diag
# ---------------------------------------------------------------------------


@ros.command("diag")
def diag() -> None:
    """Print ROS 2 diagnostics.

    Shows bridge health, VIO status (if running), container resource
    usage, node list, and topic list.
    """
    click.echo("=== ROS 2 Diagnostics ===\n")

    # Status from API
    data = _api_get("/api/ros/status")
    if data:
        click.echo(f"State:       {data.get('state', '?')}")
        click.echo(f"Middleware:   {data.get('middleware', '?')}")
        click.echo(f"Nodes:       {data.get('nodes_count', 0)}")
        click.echo(f"Topics:      {data.get('topics_count', 0)}")
        error = data.get("error")
        if error:
            click.echo(f"Error:       {error}")
    else:
        click.echo("Could not reach agent API.")

    # Container stats
    click.echo("\n--- Container Resources ---")
    try:
        result = subprocess.run(
            ["docker", "stats", "--no-stream", "--format",
             "{{.CPUPerc}}\t{{.MemUsage}}\t{{.MemPerc}}", CONTAINER_NAME],
            capture_output=True,
            timeout=10,
        )
        if result.returncode == 0:
            parts = result.stdout.decode().strip().split("\t")
            if len(parts) >= 3:
                click.echo(f"CPU:         {parts[0]}")
                click.echo(f"Memory:      {parts[1]} ({parts[2]})")
        else:
            click.echo("Container not running or docker stats failed.")
    except (subprocess.TimeoutExpired, FileNotFoundError):
        click.echo("Docker not available.")

    # Node list
    click.echo("\n--- Active Nodes ---")
    output = _ros2_cmd("node", "list")
    if output:
        for line in output.strip().splitlines():
            click.echo(f"  {line.strip()}")
    else:
        click.echo("  (none or unavailable)")

    # Topic list
    click.echo("\n--- Active Topics ---")
    output = _ros2_cmd("topic", "list", "-t")
    if output:
        for line in output.strip().splitlines():
            click.echo(f"  {line.strip()}")
    else:
        click.echo("  (none or unavailable)")

    # Bridge health
    click.echo("\n--- MAVLink Bridge Health ---")
    output = _ros2_cmd("topic", "echo", "/diagnostics", "--once", timeout=10)
    if output:
        # Just show a summary
        for line in output.strip().splitlines()[:10]:
            click.echo(f"  {line.strip()}")
    else:
        click.echo("  No diagnostic messages received (bridge may not be running).")

    # VIO status
    click.echo("\n--- VIO Status ---")
    output = _ros2_cmd("topic", "echo", "/vio/health", "--once", timeout=5)
    if output:
        for line in output.strip().splitlines()[:10]:
            click.echo(f"  {line.strip()}")
    else:
        click.echo("  VIO not running or no health topic available.")

    click.echo("")


# ---------------------------------------------------------------------------
# ados ros shell
# ---------------------------------------------------------------------------


@ros.command("shell")
def shell() -> None:
    """Open an interactive bash shell inside the ROS container.

    Sources the ROS and ADOS overlay before dropping you into the shell.
    Type 'exit' to return.
    """
    try:
        _docker_exec_interactive(
            "bash", "-c",
            "source /opt/ros/jazzy/setup.bash && "
            "source /opt/ados/ros-packages/install/setup.bash && "
            "if [ -f /root/ados_ws/install/setup.bash ]; then "
            "  source /root/ados_ws/install/setup.bash; "
            "fi && "
            "exec bash"
        )
    except FileNotFoundError:
        click.echo("Error: docker not found on PATH", err=True)
    except KeyboardInterrupt:
        pass
