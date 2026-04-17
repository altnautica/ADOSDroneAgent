"""First-boot screen shown when the node is mesh-capable but role is unset."""

from __future__ import annotations

from typing import Any


def render(draw: Any, width: int, height: int, state: dict) -> None:
    draw.text((0, 0), "ADOS Ground Agent", fill="white")
    draw.text((0, 18), "Role: unset", fill="white")
    draw.text((0, 34), "Press B3 to open menu", fill="white")
    draw.text((0, 50), "Mesh -> Set role", fill="white")
