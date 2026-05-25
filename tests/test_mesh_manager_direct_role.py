"""mesh_manager.main() must exit cleanly on a direct-role node.

A ground station in `direct` role has no mesh to bring up. The systemd
unit's ConditionPathExists gate only checks that the role sentinel file
exists, not its contents, so the process can still be launched on a
direct node. Treating that no-op as a crash (exit 2) makes systemd
Restart=on-failure flap the unit until it hits the start limit and lands
FAILED. main() must exit 0 instead.
"""

from __future__ import annotations

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
