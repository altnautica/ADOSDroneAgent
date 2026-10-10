"""mesh_manager.main() must exit cleanly on a direct-role node.

A ground station in `direct` role has no mesh to bring up. The systemd
unit's AssertPathExists gate only checks that the role sentinel file
exists, not its contents, so the process can still be launched on a
direct node. The unit is Restart=always with RestartPreventExitStatus=0, so
treating that no-op as a crash (exit 2) restarts it every RestartSec forever.
main() must exit 0 instead.
"""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest


@pytest.mark.asyncio
async def test_main_exits_zero_in_direct_role() -> None:
    """Direct role → main() exits 0 without attempting mesh setup."""
    from ados.services.ground_station import mesh_manager as mm

    cfg = MagicMock()
    with patch.object(mm, "load_config", return_value=cfg), patch.object(
        mm, "configure_logging"
    ), patch.object(mm, "get_current_role", return_value="direct"):
        with pytest.raises(SystemExit) as exc_info:
            await mm.main()

    assert exc_info.value.code == 0


@pytest.mark.skipif(sys.platform == "win32", reason="needs a unix socket")
@pytest.mark.asyncio
async def test_mesh_identity_missing_asks_supervisor(tmp_path: Path) -> None:
    """A relay with no delivered mesh identity asks the supervisor for
    `direct` and exits 0; it never writes the role sentinel itself."""
    from ados.services.ground_station import mesh_manager as mm

    received: list[dict] = []

    async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        received.append(json.loads(await reader.readline()))
        writer.write(b'{"ok": true, "role": "direct", "previous": "relay"}\n')
        await writer.drain()
        writer.close()

    sock = tmp_path / "sup.sock"
    server = await asyncio.start_unix_server(handle, path=str(sock))

    async def no_identity(_self: object) -> bool:
        raise mm.MeshIdentityMissing("mesh_id missing")

    cfg = MagicMock()
    try:
        with patch.object(mm, "load_config", return_value=cfg), patch.object(
            mm, "configure_logging"
        ), patch.object(mm, "get_current_role", return_value="relay"), patch.object(
            mm, "SUPERVISOR_SOCK", str(sock)
        ), patch.object(mm.MeshManager, "setup", no_identity):
            with pytest.raises(SystemExit) as exc_info:
                await mm.main()
    finally:
        server.close()
        await server.wait_closed()

    assert exc_info.value.code == 0
    assert received == [
        {"op": "set_role", "role": "direct", "reason": "mesh_identity_missing"}
    ]
    # The role unit no longer touches the sentinel: the supervisor owns it.
    assert not hasattr(mm, "MESH_ROLE_PATH")
