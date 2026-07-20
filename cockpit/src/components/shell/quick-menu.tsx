// The quick-menu overlay — a large-tile jump to any tab, opened by the quick
// menu command (a long-press of the panel button, the gamepad Start, or the
// action-bar ≡ button) and dismissed by back or a tap outside. Every tile is a
// generous touch target for gloved / eyes-on-panel use.

import { X } from "lucide-react";

import { useNavStore } from "@/stores/nav-store";
import { tabScreens } from "@/nav/registry";

export function QuickMenu() {
  const goTab = useNavStore((s) => s.goTab);
  const closeQuickMenu = useNavStore((s) => s.closeQuickMenu);
  const activeTabId = useNavStore((s) => s.activeTabId);
  const tabs = tabScreens();

  return (
    <div
      className="absolute inset-0 z-40 flex items-center justify-center bg-background/80 backdrop-blur-sm"
      onClick={closeQuickMenu}
    >
      <div
        className="w-[min(92%,42rem)] rounded-lg border border-border bg-surface p-[0.9rem]"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-[0.7rem] flex items-center justify-between">
          <h2 className="text-[1.05rem] font-semibold text-surface-foreground">
            Go to
          </h2>
          <button
            type="button"
            onClick={closeQuickMenu}
            aria-label="Close"
            className="touch-target flex items-center justify-center rounded-md px-[0.4rem] text-muted-foreground hover:bg-muted"
          >
            <X className="h-[1.3rem] w-[1.3rem]" />
          </button>
        </div>
        <div className="grid grid-cols-3 gap-[0.5rem] portrait:grid-cols-2">
          {tabs.map((tab) => {
            const Icon = tab.icon;
            const active = tab.id === activeTabId;
            return (
              <button
                key={tab.id}
                type="button"
                onClick={() => goTab(tab.id)}
                className={
                  "touch-target flex min-h-[4.5rem] flex-col items-center justify-center gap-[0.3rem] rounded-md " +
                  (active
                    ? "bg-amber text-amber-foreground"
                    : "bg-muted text-surface-foreground hover:bg-border")
                }
              >
                {Icon ? <Icon className="h-[1.6rem] w-[1.6rem]" aria-hidden /> : null}
                <span className="text-[0.85rem] font-medium">{tab.title}</span>
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}
