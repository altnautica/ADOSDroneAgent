// L3 — the persistent menu. A left rail in landscape, a bottom strip in
// portrait (the panel aspect decides, via Tailwind orientation variants). Each
// tab is a >=48px touch target; the active tab is amber; the button/gamepad
// focus ring highlights the focused entry so the menu is fully operable
// eyes-on-panel with any input source.

import { useNavStore } from "@/stores/nav-store";
import { tabScreens } from "@/nav/registry";
import { cn } from "@/lib/utils";

export function MenuRail({ floating = false }: { floating?: boolean }) {
  const activeTabId = useNavStore((s) => s.activeTabId);
  const menuFocusIndex = useNavStore((s) => s.menuFocusIndex);
  const goTab = useNavStore((s) => s.goTab);
  const setMenuFocus = useNavStore((s) => s.setMenuFocus);
  const tabs = tabScreens();

  return (
    <nav
      className={cn(
        "flex shrink-0 gap-[0.3rem] overflow-auto p-[0.3rem]",
        // rail (landscape) vs strip (portrait)
        "landscape:w-[6.5rem] landscape:flex-col portrait:w-full portrait:flex-row",
        floating
          ? "pointer-events-auto landscape:bg-background/45 portrait:bg-background/60 backdrop-blur-sm"
          : "landscape:border-r landscape:border-border portrait:border-t portrait:border-border bg-surface/50",
      )}
      aria-label="Cockpit menu"
    >
      {tabs.map((tab, index) => {
        const active = tab.id === activeTabId;
        const focused = index === menuFocusIndex;
        const Icon = tab.icon;
        return (
          <button
            key={tab.id}
            type="button"
            data-focused={focused || undefined}
            aria-current={active ? "page" : undefined}
            onClick={() => {
              setMenuFocus(index);
              goTab(tab.id);
            }}
            className={cn(
              "touch-target flex flex-1 flex-col items-center justify-center gap-[0.15rem] rounded-md px-[0.3rem] py-[0.35rem] transition-colors",
              active
                ? "bg-amber text-amber-foreground"
                : "text-muted-foreground hover:bg-muted hover:text-surface-foreground",
            )}
          >
            {Icon ? <Icon className="h-[1.3rem] w-[1.3rem]" aria-hidden /> : null}
            <span className="text-[0.7rem] font-medium leading-none">{tab.title}</span>
          </button>
        );
      })}
    </nav>
  );
}
