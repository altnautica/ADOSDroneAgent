"""Front-panel menu hierarchy and visibility filter.

The menu is a static tree of dicts the service walks at button-press
time. Each leaf either drives an overlay screen (``screen`` key) or
logs a ``menu_action_stub`` event for items that don't have an
implementation yet. Visibility predicates run against the live agent
state snapshot so an unsupported feature stays off-screen.
"""

from __future__ import annotations

import time
from typing import Any


# Menu tree from the physical UI spec.
# Each node is {label, children, optional: visibility, screen}. Leaves
# with a `screen` key open the named overlay. Leaves without one are
# logged as `menu_action_stub`. `visibility` is an optional callable
# `state -> bool`; when present, the node is hidden if the callable
# returns False. The filter runs against the live agent state snapshot.
MENU_TREE: list[dict[str, Any]] = [
    {"label": "Pair with drone", "children": []},
    {
        "label": "Network",
        "children": [
            {"label": "WiFi AP on/off", "children": []},
            {"label": "WiFi client scan", "children": []},
            {"label": "4G modem status", "children": []},
            {"label": "Uplink priority", "children": []},
        ],
    },
    {
        "label": "Mesh",
        # Mesh menu is always visible so operators on drone-profile or
        # direct-role nodes can see that the feature exists. When the
        # node is not mesh_capable, the submenu collapses to a single
        # hint item explaining how to enable it. This avoids a silent
        # "nothing happens when I open Mesh" failure mode.
        "children": [
            {
                "label": "Mesh unavailable",
                "children": [],
                "screen": "mesh_unavailable",
                "visibility": lambda st: not bool(
                    (st.get("role") or {}).get("mesh_capable")
                ),
            },
            {
                "label": "Set role",
                "children": [],
                "screen": "role_picker",
                "visibility": lambda st: bool(
                    (st.get("role") or {}).get("mesh_capable")
                ),
            },
            {
                "label": "Accept relay",
                "children": [],
                "screen": "accept_window",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("role") or {}).get("current") == "receiver"
                ),
            },
            {
                "label": "Join mesh",
                "children": [],
                "screen": "join_scan",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("role") or {}).get("current") == "relay"
                    and not (st.get("mesh") or {}).get("up")
                ),
            },
            {
                "label": "Neighbors",
                "children": [],
                "screen": "neighbors",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("role") or {}).get("current") in ("relay", "receiver")
                ),
            },
            {
                "label": "Leave mesh",
                "children": [],
                "screen": "leave_confirm",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("mesh") or {}).get("up", False)
                ),
            },
        ],
    },
    {
        "label": "Radio",
        "children": [
            {"label": "Channel", "children": []},
            {"label": "TX power (n/a)", "children": []},
            {"label": "Bitrate profile", "children": []},
        ],
    },
    {
        "label": "Display",
        "children": [
            {"label": "HDMI resolution", "children": []},
            {"label": "OLED brightness", "children": []},
        ],
    },
    {
        "label": "System",
        "children": [
            {"label": "Version", "children": []},
            {"label": "Reboot", "children": []},
            {"label": "Factory reset", "children": []},
        ],
    },
    {"label": "Back to status", "children": []},
]


def _filter_visible(items: list[dict[str, Any]], state: dict) -> list[dict[str, Any]]:
    """Drop items whose `visibility` callable returns False.

    Nodes without a visibility callable are always visible. Errors in
    the callable are treated as "hide" so a broken predicate does not
    crash the menu.
    """
    out: list[dict[str, Any]] = []
    for node in items:
        vis = node.get("visibility")
        if vis is None:
            out.append(node)
            continue
        try:
            if vis(state):
                out.append(node)
        except Exception:
            pass
    return out


def _now() -> float:
    return time.monotonic()


def _normalize_radio_fields(data: dict[str, Any]) -> dict[str, Any]:
    """Backfill ``link.tx_power_dbm`` and ``radio.topology`` defaults.

    The dashboard tile + OLED link screen both reach into
    ``state["link"]["tx_power_dbm"]`` and
    ``state["radio"]["topology"]``. Older agent builds (or any build
    that hasn't taken the WFB-status REST exposure yet) won't carry
    these keys; left missing, the renderers paint ``--`` placeholders.
    Filling defensively here means downstream code can rely on a
    stable shape regardless of which side ships first.

    Mutates and returns the same dict for caller convenience. The
    transformation is shallow and additive — we never overwrite a
    value the agent already supplied.
    """
    link = data.get("link")
    if isinstance(link, dict):
        link.setdefault("tx_power_dbm", None)
    radio = data.get("radio")
    if not isinstance(radio, dict):
        radio = {}
        data["radio"] = radio
    radio.setdefault("topology", "host_vbus")
    return data
