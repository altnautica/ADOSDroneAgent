"""Suite management routes — list, activate, deactivate suites."""

from __future__ import annotations

import os
from pathlib import Path

import yaml
from fastapi import APIRouter

router = APIRouter()

SUITE_DIRS = [
    Path("/opt/ados/suites"),
    Path("/etc/ados/suites"),
]


def _find_suites() -> list[dict]:
    """Scan suite directories for YAML manifests."""
    suites = []
    seen = set()
    for base in SUITE_DIRS:
        if not base.exists():
            continue
        for f in sorted(base.glob("*.yaml")):
            suite_id = f.stem
            if suite_id in seen:
                continue
            seen.add(suite_id)
            try:
                with open(f) as fh:
                    manifest = yaml.safe_load(fh) or {}
                suites.append({
                    "id": suite_id,
                    "name": manifest.get("name", suite_id),
                    "description": manifest.get("description", ""),
                    "icon": manifest.get("icon", "box"),
                    "sensorsRequired": manifest.get("sensors_required", []),
                    "tierRequired": manifest.get("tier_required", 1),
                    "version": manifest.get("version", "1.0.0"),
                    "installed": True,
                    "active": _is_active(suite_id),
                    "category": manifest.get("category", "general"),
                    "requiredServices": manifest.get("required_services", []),
                })
            except Exception:
                pass
    return suites


def _is_active(suite_id: str) -> bool:
    """Check if a suite is currently active (reads from config or state file)."""
    state_file = Path("/var/ados/state/active_suite")
    if state_file.exists():
        return state_file.read_text().strip() == suite_id
    return False


def _set_active(suite_id: str | None) -> None:
    """Record the active suite in state file."""
    state_dir = Path("/var/ados/state")
    state_dir.mkdir(parents=True, exist_ok=True)
    state_file = state_dir / "active_suite"
    if suite_id:
        state_file.write_text(suite_id)
    elif state_file.exists():
        state_file.unlink()


@router.get("/suites")
async def list_suites():
    """List available suites with activation status."""
    return _find_suites()


@router.post("/suites/{suite_id}/activate")
async def activate_suite(suite_id: str):
    """Activate a suite — starts required services via supervisor."""
    import subprocess

    # Check suite exists
    suites = {s["id"]: s for s in _find_suites()}
    if suite_id not in suites:
        return {"status": "error", "message": f"Suite not found: {suite_id}"}

    suite = suites[suite_id]

    # Start required services
    started = []
    for svc in suite.get("requiredServices", []):
        svc_name = f"ados-{svc}" if not svc.startswith("ados-") else svc
        try:
            result = subprocess.run(
                ["systemctl", "start", svc_name],
                capture_output=True, text=True, timeout=15,
            )
            if result.returncode == 0:
                started.append(svc_name)
        except Exception:
            pass

    _set_active(suite_id)
    return {
        "status": "ok",
        "message": f"Suite {suite_id} activated",
        "servicesStarted": started,
    }


@router.post("/suites/{suite_id}/deactivate")
async def deactivate_suite(suite_id: str):
    """Deactivate a suite — stops suite-specific services."""
    import subprocess

    suites = {s["id"]: s for s in _find_suites()}
    suite = suites.get(suite_id)
    if not suite:
        return {"status": "error", "message": f"Suite not found: {suite_id}"}

    stopped = []
    for svc in suite.get("requiredServices", []):
        svc_name = f"ados-{svc}" if not svc.startswith("ados-") else svc
        try:
            subprocess.run(
                ["systemctl", "stop", svc_name],
                capture_output=True, text=True, timeout=15,
            )
            stopped.append(svc_name)
        except Exception:
            pass

    _set_active(None)
    return {
        "status": "ok",
        "message": f"Suite {suite_id} deactivated",
        "servicesStopped": stopped,
    }


@router.post("/suites/{suite_id}/install")
async def install_suite(suite_id: str):
    """Install a suite from registry (future)."""
    return {"status": "not_implemented", "message": f"Suite install from registry not yet available: {suite_id}"}


@router.post("/suites/{suite_id}/uninstall")
async def uninstall_suite(suite_id: str):
    """Uninstall a suite (future)."""
    return {"status": "not_implemented", "message": f"Suite uninstall not yet available: {suite_id}"}
