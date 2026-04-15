"""Menu screen: list of items with current selection inverted."""

from __future__ import annotations

from typing import Any


ROW_HEIGHT = 12
VISIBLE_ROWS = 5


def render(draw: Any, width: int, height: int, state: dict) -> None:
    items: list[str] = state.get("items") or []
    selected: int = int(state.get("selected") or 0)
    depth: int = int(state.get("depth") or 0)

    draw.text((0, 0), f"MENU {'>' * max(depth, 0)}", fill="white")

    if not items:
        draw.text((0, 16), "(empty)", fill="white")
        return

    # Window the list so the selection stays visible.
    half = VISIBLE_ROWS // 2
    start = max(0, selected - half)
    end = min(len(items), start + VISIBLE_ROWS)
    start = max(0, end - VISIBLE_ROWS)

    y = 14
    for i in range(start, end):
        label = items[i][:20]
        if i == selected:
            # Inverse row: filled rectangle with black text.
            draw.rectangle((0, y - 1, width - 1, y + ROW_HEIGHT - 2), fill="white")
            draw.text((2, y), label, fill="black")
        else:
            draw.text((2, y), label, fill="white")
        y += ROW_HEIGHT
