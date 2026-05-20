"""``ados profile`` CLI subcommand tree.

Operator-facing override for the agent profile. Until this command
existed, flipping a node from `drone` to `ground_station` (or back)
required either re-running ``install.sh --profile <value>`` or
hand-editing ``/etc/ados/config.yaml``. Both are friction-heavy and
the latter violates the repo-first / never-edit-/etc/ados rule for
operators who don't know the layout.

Two commands:

* ``ados profile show`` — print the current resolution chain and the
  resolved value the agent will report.
* ``ados profile set <profile>`` — persist the operator's choice to
  ``/etc/ados/profile.conf`` and ``/etc/ados/config.yaml`` agent.profile
  so the next agent restart honors it. The command itself does NOT
  restart the agent; the operator runs ``sudo systemctl restart
  ados-supervisor`` (or reboots) when ready.
"""

from __future__ import annotations

import json
from pathlib import Path

import click

from ados.core.paths import CONFIG_YAML, PROFILE_CONF
from ados.core.profile import _read_profile_conf_value, current_profile_and_role


_VALID = ("drone", "ground_station")


@click.group("profile", help="Show or override the agent profile.")
def profile_group() -> None:
    pass


@profile_group.command("show", help="Print the resolved agent profile.")
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def show(as_json: bool) -> None:
    config_value = _read_config_yaml_profile()
    conf_value = _read_profile_conf_value()
    # Reuse the same resolver the heartbeat + status routes use so
    # what the operator sees here is exactly what the wire reports.
    resolved_profile, resolved_role = current_profile_and_role(_StubConfig(config_value))
    payload = {
        "config_yaml": config_value,
        "profile_conf": conf_value,
        "resolved_profile": resolved_profile,
        "resolved_role": resolved_role,
    }
    if as_json:
        click.echo(json.dumps(payload))
        return
    click.echo(f"config.yaml agent.profile: {config_value or '(unset)'}")
    click.echo(f"/etc/ados/profile.conf:    {conf_value or '(missing)'}")
    click.echo(f"resolved profile (wire):   {resolved_profile}")
    if resolved_role is not None:
        click.echo(f"resolved role:             {resolved_role}")


@profile_group.command("set", help="Persist a profile choice for this node.")
@click.argument("value", type=click.Choice(list(_VALID) + ["ground-station"]))
@click.option(
    "--role",
    type=click.Choice(["direct", "relay", "receiver"]),
    default=None,
    help="Mesh role for ground_station; ignored for drone. Defaults to direct.",
)
@click.option(
    "--no-restart",
    is_flag=True,
    default=False,
    help=(
        "Skip the automatic systemctl restart of profile-sensitive units. "
        "Operator is responsible for restarting the agent before the new "
        "profile takes effect."
    ),
)
@click.option("--json", "as_json", is_flag=True, help="Machine-readable output.")
def set_profile(value: str, role: str | None, no_restart: bool, as_json: bool) -> None:
    target = "ground_station" if value in ("ground_station", "ground-station") else "drone"
    role_to_persist = (role or "direct") if target == "ground_station" else None

    try:
        _write_profile_conf(target)
        _write_config_yaml(target, role_to_persist)
    except PermissionError as exc:
        msg = (
            f"Permission denied writing to {exc.filename}. "
            "Re-run with sudo: sudo ados profile set ..."
        )
        if as_json:
            click.echo(json.dumps({"ok": False, "error": "permission_denied", "message": msg}))
            raise SystemExit(1)
        click.echo(f"error: {msg}", err=True)
        raise SystemExit(1)

    # Restart profile-sensitive units so the new gate (in wfb/__main__.py
    # and friends) is picked up without a manual second step. Without
    # this, a `profile set ground_station` leaves ados-wfb running the
    # drone-side WfbManager from the prior profile, racing wfb-stats.json
    # with the about-to-start ados-wfb-rx unit on next reload.
    restart_attempts: list[dict] = []
    if not no_restart:
        import subprocess

        # Order: stop wfb first to release the radio, then restart the
        # profile-specific RX unit on ground. supervisor wraps everything
        # else so the bulk of the agent restarts cleanly.
        candidates = ["ados-wfb", "ados-wfb-rx", "ados-supervisor"]
        for unit in candidates:
            try:
                r = subprocess.run(
                    ["systemctl", "restart", unit],
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
                restart_attempts.append({
                    "unit": unit,
                    "returncode": r.returncode,
                    "stderr": (r.stderr or "").strip()[:200],
                })
            except subprocess.SubprocessError as exc:
                restart_attempts.append({
                    "unit": unit,
                    "returncode": -1,
                    "stderr": str(exc)[:200],
                })

    payload = {
        "ok": True,
        "profile": target,
        "role": role_to_persist,
        "restart_required": no_restart,
        "restarted_units": restart_attempts,
        "restart_hint": (
            "sudo systemctl restart ados-supervisor"
            if no_restart
            else None
        ),
    }
    if as_json:
        click.echo(json.dumps(payload))
        return
    label = (
        f"{target} (role: {role_to_persist})"
        if role_to_persist
        else target
    )
    click.echo(f"Profile set to {label}.")
    if no_restart:
        click.echo("Restart the agent to apply: sudo systemctl restart ados-supervisor")
    else:
        successes = [r["unit"] for r in restart_attempts if r["returncode"] == 0]
        failures = [r for r in restart_attempts if r["returncode"] != 0]
        if successes:
            click.echo(f"Restarted: {', '.join(successes)}")
        for f in failures:
            click.echo(
                f"warn: failed to restart {f['unit']}: {f['stderr']}",
                err=True,
            )


# ---------------------------------------------------------------------------
# Helpers — kept private to this module so the CLI surface stays small.
# ---------------------------------------------------------------------------


class _StubConfig:
    """Minimal stand-in for the runtime config so we can reuse
    current_profile_and_role without importing the full Pydantic
    model + its dependency tree from a CLI process."""

    def __init__(self, profile_value: str | None) -> None:
        class _Agent:
            pass

        self.agent = _Agent()
        self.agent.profile = profile_value or "auto"


def _read_config_yaml_profile() -> str | None:
    if not Path(CONFIG_YAML).exists():
        return None
    try:
        import yaml
    except ImportError:
        return None
    try:
        data = yaml.safe_load(Path(CONFIG_YAML).read_text(encoding="utf-8")) or {}
    except (OSError, yaml.YAMLError):
        return None
    if not isinstance(data, dict):
        return None
    agent = data.get("agent") or {}
    if not isinstance(agent, dict):
        return None
    value = agent.get("profile")
    return str(value) if isinstance(value, str) else None


def _write_profile_conf(target: str) -> None:
    """Atomic write of the canonical profile sentinel file."""
    path = Path(PROFILE_CONF)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(f"profile: {target}\n", encoding="utf-8")
    tmp.replace(path)


def _write_config_yaml(target: str, role: str | None) -> None:
    """Persist agent.profile (and optional ground_station.role) to config.yaml.

    Idempotent: never wipes unrelated keys, only sets the targeted
    fields. Uses pyyaml in safe-load/safe-dump mode.
    """
    try:
        import yaml
    except ImportError as exc:
        raise RuntimeError("pyyaml is required to update config.yaml") from exc

    path = Path(CONFIG_YAML)
    if path.exists():
        try:
            data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        except (OSError, yaml.YAMLError):
            data = {}
    else:
        data = {}

    if not isinstance(data, dict):
        data = {}

    agent = data.setdefault("agent", {})
    if isinstance(agent, dict):
        agent["profile"] = target

    if target == "ground_station" and role is not None:
        gs = data.setdefault("ground_station", {})
        if isinstance(gs, dict):
            gs.setdefault("role", role)
            gs.setdefault("mesh_capable", False)

    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(
        yaml.safe_dump(data, sort_keys=False, default_flow_style=False),
        encoding="utf-8",
    )
    tmp.replace(path)
