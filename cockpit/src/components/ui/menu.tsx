// A popup menu under its anchor (flipped above it when there is no room
// below) over a clear scrim. Tapping outside, Escape and the controller back
// close it; an item's action runs after the menu has closed. Mirrors the
// Android console menu.

import type { LucideIcon } from "lucide-react";

import { ListItem } from "@/components/ui/list-item";
import { Overlay } from "@/components/ui/overlay";
import { MENU_MAX_WIDTH_PX, MENU_MIN_WIDTH_PX, placeMenu, type AnchorRect } from "@/lib/menu-placement";

export interface MenuItem {
  label: string;
  icon?: LucideIcon;
  enabled?: boolean;
  onSelect: () => void;
}

export function Menu({
  anchor,
  items,
  onClose,
}: {
  /** The anchor's bounds (`getBoundingClientRect()`), or null for the corner. */
  anchor: AnchorRect | null;
  items: MenuItem[];
  onClose: () => void;
}) {
  const place = placeMenu(anchor, window.innerWidth, window.innerHeight);
  return (
    <Overlay
      onDismiss={onClose}
      scrim="clear"
      panelClassName="absolute overflow-y-auto rounded-lg border border-border bg-surface py-[0.25rem] text-surface-foreground shadow-lg"
      panelStyle={{
        left: place.left,
        top: place.top,
        bottom: place.bottom,
        maxHeight: place.maxHeight,
        minWidth: MENU_MIN_WIDTH_PX,
        maxWidth: MENU_MAX_WIDTH_PX,
      }}
    >
      {items.map((item) => (
        <ListItem
          key={item.label}
          headline={item.label}
          icon={item.icon}
          enabled={item.enabled ?? true}
          onClick={() => {
            onClose();
            item.onSelect();
          }}
        />
      ))}
    </Overlay>
  );
}
