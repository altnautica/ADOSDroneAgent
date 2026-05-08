"""Detail (drilldown) pages pushed as modals from the dashboard.

Each detail page lives in its own module (``radio_link``, ``drone``,
``mesh``, ``uplink``) so the file budget stays well under the soft
LOC ceiling and so a single page can be unit-tested without dragging
in its three siblings.

The dashboard tile-tap handler imports the page classes from these
modules and pushes them onto the page navigator's modal stack. The
:data:`DETAIL_PAGES` registry is exported here for tests + tooling
that want to enumerate the full set without reflecting on the
dashboard.
"""

from __future__ import annotations

from .drone import DroneDetailPage
from .mesh import MeshDetailPage
from .radio_link import RadioLinkDetailPage
from .uplink import UplinkDetailPage

DETAIL_PAGES: dict[str, type] = {
    "details.radio_link": RadioLinkDetailPage,
    "details.drone": DroneDetailPage,
    "details.mesh": MeshDetailPage,
    "details.uplink": UplinkDetailPage,
}

__all__ = [
    "DETAIL_PAGES",
    "DroneDetailPage",
    "MeshDetailPage",
    "RadioLinkDetailPage",
    "UplinkDetailPage",
]
