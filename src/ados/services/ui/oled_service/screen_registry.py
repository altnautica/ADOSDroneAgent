"""Registries that map screen ids to their renderer modules.

Both the auto-cycle status carousel (:data:`SCREEN_RENDERERS`) and
the mesh overlays (:data:`OVERLAY_SCREENS`) live here so the service
module can import them as a single contract object.
"""

from __future__ import annotations

from typing import Any

from ados.services.ui.screens import (
    drone as screen_drone,
)
from ados.services.ui.screens import (
    gcs as screen_gcs,
)
from ados.services.ui.screens import (
    link as screen_link,
)
from ados.services.ui.screens import (
    menu as screen_menu,
)
from ados.services.ui.screens import (
    net as screen_net,
)
from ados.services.ui.screens import (
    system as screen_system,
)
from ados.services.ui.screens.mesh import (
    accept_window as screen_mesh_accept_window,
)
from ados.services.ui.screens.mesh import (
    error_states as screen_mesh_error_states,
)
from ados.services.ui.screens.mesh import (
    hub_unreachable as screen_mesh_hub_unreachable,
)
from ados.services.ui.screens.mesh import (
    join_request_inflight as screen_mesh_join_request_inflight,
)
from ados.services.ui.screens.mesh import (
    join_scan as screen_mesh_join_scan,
)
from ados.services.ui.screens.mesh import (
    joined_status as screen_mesh_joined_status,
)
from ados.services.ui.screens.mesh import (
    leave_confirm as screen_mesh_leave_confirm,
)
from ados.services.ui.screens.mesh import (
    mesh_unavailable as screen_mesh_unavailable,
)
from ados.services.ui.screens.mesh import (
    neighbors as screen_mesh_neighbors,
)
from ados.services.ui.screens.mesh import (
    role_picker as screen_mesh_role_picker,
)
from ados.services.ui.screens.mesh import (
    unset_boot as screen_mesh_unset_boot,
)

# Screen registry keyed by screen id so the active list can be rebuilt
# from `ground_station.ui.screens` config. REST schema uses the keys
# `home`, `link`, `drone`, `network`, `system`, `qr`. We map older ids
# onto the current REST schema (`net` -> `network`, no separate `home`
# or `qr` renderer yet) so an unknown screen id is silently skipped
# instead of crashing.
SCREEN_RENDERERS: dict[str, Any] = {
    "link": screen_link,
    "drone": screen_drone,
    "gcs": screen_gcs,
    "net": screen_net,
    "network": screen_net,
    "system": screen_system,
}

DEFAULT_SCREEN_ORDER: list[str] = ["link", "drone", "gcs", "net", "system"]
DEFAULT_SCREEN_ENABLED: list[str] = ["link", "drone", "gcs", "net", "system"]

# Overlay screen registry. Each module exports render() plus optional
# BUTTON_ACTIONS, initial_state(service), on_enter(service), and
# on_exit(service). The service dispatches button presses to
# BUTTON_ACTIONS while the overlay is active. B4 in an overlay always
# exits unless the module maps B4 to a different action.
OVERLAY_SCREENS: dict[str, Any] = {
    "unset_boot": screen_mesh_unset_boot,
    "role_picker": screen_mesh_role_picker,
    "accept_window": screen_mesh_accept_window,
    "join_scan": screen_mesh_join_scan,
    "join_request_inflight": screen_mesh_join_request_inflight,
    "joined_status": screen_mesh_joined_status,
    "hub_unreachable": screen_mesh_hub_unreachable,
    "mesh_unavailable": screen_mesh_unavailable,
    "neighbors": screen_mesh_neighbors,
    "leave_confirm": screen_mesh_leave_confirm,
    "error_states": screen_mesh_error_states,
}

# Direct re-export for the first-boot render path in the service so it
# can skip a dict lookup on every tick.
unset_boot_screen = screen_mesh_unset_boot
