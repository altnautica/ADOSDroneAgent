"""Cloud-posture and Cloudflare tunnel-token helpers.

The first half reads the current cloud posture out of config for
display. The second half persists operator-supplied cloud config
(posture choice, self-hosted backend URL, tunnel token) to the
on-disk config + root-owned secret files.
"""

from __future__ import annotations

import os
import shutil
from pathlib import Path
from typing import Any

from ados.setup.models import CloudChoiceStatus, SetupActionResult

from ._constants import _TOKEN_RE


def extract_cloudflare_token(value: str) -> str:
    """Extract a tunnel token from a raw token or Cloudflare install command."""
    candidate = value.strip()
    match = _TOKEN_RE.search(candidate)
    if match:
        candidate = match.group(1)
    candidate = candidate.strip().strip("'\"")
    if not candidate or any(ch.isspace() for ch in candidate):
        raise ValueError("Cloudflare tunnel token could not be found")
    if len(candidate) < 20:
        raise ValueError("Cloudflare tunnel token is too short")
    return candidate


def _cloud_choice_status(config: Any) -> CloudChoiceStatus:
    """Read the current cloud posture out of config for display."""
    server = getattr(config, "server", None)
    mode = getattr(server, "mode", "local") if server else "local"
    if mode not in ("cloud", "self_hosted", "local"):
        mode = "local"
    if mode == "local":
        return CloudChoiceStatus(
            mode="local",
            paired=False,
            pair_code_required=False,
            backend_url="",
            backend_reachable=False,
        )
    if mode == "self_hosted":
        sh = getattr(server, "self_hosted", None)
        url = str(getattr(sh, "url", "") or "")
        return CloudChoiceStatus(
            mode="self_hosted",
            paired=bool(getattr(sh, "api_key", "") or ""),
            pair_code_required=True,
            backend_url=url,
            backend_reachable=False,
        )
    cloud = getattr(server, "cloud", None)
    cloud_url = str(getattr(cloud, "url", "") or "")
    return CloudChoiceStatus(
        mode="cloud",
        paired=False,
        pair_code_required=True,
        backend_url=cloud_url,
        backend_reachable=False,
    )


def apply_cloud_choice(  # noqa: C901
    runtime: Any,
    *,
    mode: str,
    self_hosted: dict[str, Any] | None = None,
) -> SetupActionResult:
    """Apply a cloud-posture choice to ``config.server``.

    Persists the chosen mode and any self-hosted backend coordinates the
    operator entered. The optional ``api_key`` is written to a root-owned
    secret file and is not stored back in config or returned in the
    response. ``mqtt_password`` is cleared on transition to ``local``.
    """
    if mode not in ("cloud", "self_hosted", "local"):
        return SetupActionResult(ok=False, message=f"Unknown mode: {mode}")

    if mode == "self_hosted":
        if not self_hosted or not self_hosted.get("url"):
            return SetupActionResult(
                ok=False,
                message="self_hosted.url is required when mode is 'self_hosted'",
            )
    elif self_hosted:
        return SetupActionResult(
            ok=False,
            message="self_hosted block is only valid when mode is 'self_hosted'",
        )

    config = runtime.config
    config.server.mode = mode

    api_key_written = False
    if mode == "self_hosted":
        sh = config.server.self_hosted
        sh.url = str(self_hosted.get("url") or "").strip()
        sh.mqtt_broker = str(self_hosted.get("mqtt_broker") or "").strip()
        port_raw = self_hosted.get("mqtt_port")
        if port_raw is not None:
            try:
                port_int = int(port_raw)
            except (TypeError, ValueError):
                return SetupActionResult(
                    ok=False, message="self_hosted.mqtt_port must be an integer"
                )
            if not (1 <= port_int <= 65535):
                return SetupActionResult(
                    ok=False, message="self_hosted.mqtt_port must be 1-65535"
                )
            sh.mqtt_port = port_int
        api_key = self_hosted.get("api_key")
        if api_key:
            try:
                from ados.core.paths import SERVER_API_KEY_PATH
                SERVER_API_KEY_PATH.parent.mkdir(parents=True, exist_ok=True)
                fd = os.open(
                    str(SERVER_API_KEY_PATH),
                    os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
                    0o600,
                )
                with os.fdopen(fd, "w", encoding="utf-8") as fh:
                    fh.write(str(api_key).strip())
                    fh.write("\n")
                api_key_written = True
                sh.api_key = ""  # never echo back through config
            except OSError as exc:
                return SetupActionResult(
                    ok=False, message=f"Could not write API key: {exc}"
                )

    if mode == "local":
        config.server.mqtt_password = ""

    saver = getattr(runtime.raw_runtime, "save_config", None)
    if callable(saver):
        try:
            saver()
        except Exception:
            pass

    data: dict[str, object] = {
        "mode": mode,
        "api_key_written": api_key_written,
    }
    if mode == "cloud":
        data["backend_url"] = config.server.cloud.url
    elif mode == "self_hosted":
        data["backend_url"] = config.server.self_hosted.url

    if mode == "local":
        message = "Cloud posture set to local-only. Mission Control connects directly."
    elif mode == "cloud":
        message = "Cloud posture set to Altnautica cloud. Continue to pairing."
    else:
        message = "Cloud posture set to self-hosted backend. Continue to pairing."

    return SetupActionResult(ok=True, message=message, data=data)


def install_cloudflare_token(runtime: Any, token_or_script: str) -> SetupActionResult:
    """Persist a Cloudflare tunnel token and mark remote access enabled."""
    token = extract_cloudflare_token(token_or_script)
    cf = runtime.config.remote_access.cloudflare
    token_path = Path(cf.token_path)
    try:
        token_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(str(token_path), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(token)
            fh.write("\n")
    except OSError as exc:
        return SetupActionResult(ok=False, message=f"Could not write token: {exc}")

    runtime.config.remote_access.provider = "cloudflare"
    cf.enabled = True
    saver = getattr(runtime.raw_runtime, "save_config", None)
    if callable(saver):
        try:
            saver()
        except Exception:
            pass

    data: dict[str, object] = {
        "token_path": str(token_path),
        "cloudflared_installed": bool(shutil.which("cloudflared")),
    }
    if shutil.which("systemctl"):
        data["service_command"] = f"sudo systemctl restart {cf.service_name}"
    return SetupActionResult(
        ok=True,
        message="Cloudflare tunnel token installed. Restart cloudflared to connect the tunnel.",
        data=data,
    )


__all__ = [
    "extract_cloudflare_token",
    "_cloud_choice_status",
    "apply_cloud_choice",
    "install_cloudflare_token",
]
