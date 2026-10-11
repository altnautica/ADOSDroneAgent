"""Mesh identity (mesh_id + shared PSK) and mesh interface selection."""

from __future__ import annotations

import hashlib
import os
import secrets
from pathlib import Path

from ados.core.config import ADOSConfig
from ados.core.paths import MESH_ID_PATH


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


def _pick_mesh_iface(configured: str | None) -> str | None:
    """Return the wireless interface batman-adv should bind to.

    Priority: explicit config > the mesh role from the single driver-keyed
    role resolver. The resolver never assigns the WFB flight radio to the
    mesh (it is by construction a non-WFB radio), and decides by the bound
    kernel driver, not by the racing interface name.
    """
    if configured:
        return configured

    from ados.services.network.interface_roles import assign_roles

    roles = assign_roles()
    # mesh is the second non-WFB radio; fall back to the single control radio
    # (mgmt_wifi) when the box has exactly one non-WFB adapter.
    return roles.mesh or roles.mgmt_wifi
