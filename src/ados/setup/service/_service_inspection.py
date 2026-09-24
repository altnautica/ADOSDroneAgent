"""Service state + remote-access status helpers."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Any, Literal

from ados.setup.models import RemoteAccessStatus


def _cloudflared_running(service_name: str) -> bool:
    if shutil.which("systemctl"):
        try:
            result = subprocess.run(
                ["systemctl", "is-active", "--quiet", service_name],
                capture_output=True,
                timeout=3,
            )
            return result.returncode == 0
        except (OSError, subprocess.SubprocessError):
            return False
    return False


def _remote_status(config: Any) -> RemoteAccessStatus:
    remote = config.remote_access
    cf = remote.cloudflare
    public_urls = list(remote.public_urls)
    for url in (cf.setup_url, cf.api_url, cf.video_whep_url, cf.mavlink_ws_url):
        if url and url not in public_urls:
            public_urls.append(url)

    configured = bool(cf.enabled and Path(cf.token_path).is_file())
    status: Literal["disabled", "configured", "running", "stopped", "error"] = "disabled"
    error = ""
    if cf.enabled:
        status = "configured" if configured else "error"
        if not configured:
            error = "Cloudflare tunnel is enabled but no token is installed"
        elif _cloudflared_running(cf.service_name):
            status = "running"
        else:
            status = "stopped"

    return RemoteAccessStatus(
        provider=remote.provider,
        enabled=bool(cf.enabled),
        configured=configured,
        status=status,
        public_urls=public_urls,
        error=error,
    )


__all__ = [
    "_cloudflared_running",
    "_remote_status",
]
