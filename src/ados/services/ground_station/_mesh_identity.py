"""Mesh identity (mesh_id + shared PSK) and mesh interface selection."""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import time
from pathlib import Path

from ados.core.config import ADOSConfig
from ados.core.paths import ADOS_RUN_DIR, MESH_ID_PATH

# The role sidecars the native relay / receiver loops write every couple of
# seconds. Each carries `mesh_adapter`: the WFB adapter the native adapter-role
# resolver left to the mesh carrier (role `mesh` in `radio.adapters`, or the
# last adapter under automatic roles).
_ROLE_SIDECARS = {"relay": "wfb-relay.json", "receiver": "wfb-receiver.json"}
# The registry freshness window for both sidecars. An older file is a stopped
# writer, not a current assignment.
_ROLE_SIDECAR_MAX_AGE_S = 10.0


class MeshIdentityMissing(RuntimeError):
    """A relay role was requested but the deployment mesh identity
    (mesh_id + PSK) has not been delivered via a pairing invite yet.
    The caller should fall back to `direct` rather than crash-loop."""


def _ensure_mesh_identity(role: str, config: ADOSConfig) -> tuple[str, bytes]:
    """Load or create the deployment mesh_id + shared PSK.

    On a receiver node the first boot generates both and writes them to
    `/etc/ados/mesh/`. Relays pick these values up from the pairing
    invite bundle written by `pairing_manager`. If the files are missing
    on a relay we raise `MeshIdentityMissing` so the caller can surface
    an OLED error state and downgrade role instead of crash-looping.
    """
    MESH_ID_PATH.parent.mkdir(parents=True, exist_ok=True)

    configured_id = config.ground_station.mesh.mesh_id
    if configured_id:
        mesh_id = configured_id
    elif MESH_ID_PATH.is_file():
        mesh_id = MESH_ID_PATH.read_text(encoding="utf-8").strip()
    elif role == "receiver":
        # Derive a stable short id from the device_id. HKDF would be
        # overkill for a 16-char mesh SSID; a SHA-256 truncation keeps
        # it deterministic per device.
        seed = config.agent.device_id or secrets.token_hex(8)
        mesh_id = "ados-" + hashlib.sha256(seed.encode()).hexdigest()[:10]
        MESH_ID_PATH.write_text(mesh_id + "\n", encoding="utf-8")
        os.chmod(MESH_ID_PATH, 0o644)
    else:
        raise MeshIdentityMissing(
            "mesh_id missing. A relay must be paired with a receiver before "
            "mesh_manager can start."
        )

    psk_path = Path(config.ground_station.mesh.shared_key_path)
    if psk_path.is_file():
        psk = psk_path.read_bytes().strip()
        if len(psk) < 16:
            raise RuntimeError(
                f"mesh PSK at {psk_path} is shorter than 16 bytes"
            )
    elif role == "receiver":
        psk = secrets.token_bytes(32)
        psk_path.parent.mkdir(parents=True, exist_ok=True)
        psk_path.write_bytes(psk)
        os.chmod(psk_path, 0o600)
    else:
        raise MeshIdentityMissing(
            f"mesh PSK missing at {psk_path}. A relay must be paired before "
            "mesh_manager can start."
        )

    return mesh_id, psk


def _published_mesh_adapter(
    role: str,
    run_dir: Path | None = None,
    now: float | None = None,
) -> str | None:
    """The WFB adapter the native resolver assigned to the mesh, read from this
    role's sidecar, or None when the sidecar is absent, stale, unparseable or
    names no adapter."""
    name = _ROLE_SIDECARS.get(role)
    if name is None:
        return None
    path = (run_dir or ADOS_RUN_DIR) / name
    try:
        age = (time.time() if now is None else now) - path.stat().st_mtime
        body = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    if not 0 <= age <= _ROLE_SIDECAR_MAX_AGE_S or not isinstance(body, dict):
        return None
    adapter = body.get("mesh_adapter")
    return adapter if isinstance(adapter, str) and adapter else None


def _pick_mesh_iface(configured: str | None, role: str) -> str | None:
    """Return the wireless interface batman-adv should bind to.

    Priority: explicit config > the WFB adapter the native adapter-role
    resolver assigned to the mesh (published on this role's sidecar) > the mesh
    role from the single driver-keyed role resolver. That last resolver never
    assigns the WFB flight radio to the mesh (it is by construction a non-WFB
    radio), and decides by the bound kernel driver, not by the racing
    interface name.
    """
    if configured:
        return configured
    published = _published_mesh_adapter(role)
    if published:
        return published

    from ados.services.network.interface_roles import assign_roles

    roles = assign_roles()
    # mesh is the second non-WFB radio; fall back to the single control radio
    # (mgmt_wifi) when the box has exactly one non-WFB adapter.
    return roles.mesh or roles.mgmt_wifi
