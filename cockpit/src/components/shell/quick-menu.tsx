// The quick menu: a modal of large tiles to jump to any tab, the running
// extensions' cockpit panels, and the way back to the node dashboard. Opened
// by the quick-menu command (panel long-press, gamepad Start, the Menu
// button); closed by back, Escape or a tap outside. Focus is trapped inside
// while it is open and returns to where it was on close. The exit is hidden
// on the kiosk (`?kiosk=1`), where the cockpit is the whole display.

import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { ArrowLeft, LayoutDashboard, X } from "lucide-react";

import { mountablePanels } from "@/components/plugins/extension-feed-mounts";
import { PluginFrameHost } from "@/components/plugins/plugin-overlay-host";
import { isKiosk } from "@/lib/kiosk";
import { tabScreens } from "@/nav/registry";
import { useProfile } from "@/shared/use-profile";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useNavStore } from "@/stores/nav-store";
import { cn } from "@/lib/utils";

const FOCUSABLE = 'button:not([disabled]), [href], iframe, [tabindex]:not([tabindex="-1"])';

export function QuickMenu() {
  const goTab = useNavStore((s) => s.goTab);
  const closeQuickMenu = useNavStore((s) => s.closeQuickMenu);
  const activeTabId = useNavStore((s) => s.activeTabId);
  const tabs = tabScreens(useProfile());
  const extensions = useExtensionsStore((s) => s.extensions);
  const panels = extensions.flatMap((ext) => mountablePanels(ext, "cockpit.panel").map((p) => ({ ext, p })));
  const [openPanel, setOpenPanel] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const kiosk = isKiosk();

  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    dialogRef.current?.querySelector<HTMLElement>(FOCUSABLE)?.focus();
    return () => previous?.focus?.();
  }, []);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      closeQuickMenu();
      return;
    }
    if (e.key !== "Tab") return;
    const nodes = Array.from(dialogRef.current?.querySelectorAll<HTMLElement>(FOCUSABLE) ?? []);
    if (nodes.length === 0) return;
    const first = nodes[0];
    const last = nodes[nodes.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  };

  const shown = panels.find(({ ext, p }) => `${ext.pluginId}:${p.id}` === openPanel) ?? null;

  return (
    <div
      className="absolute inset-0 z-50 flex items-center justify-center bg-scrim backdrop-blur-sm"
      onClick={closeQuickMenu}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="quick-menu-title"
        onKeyDown={onKeyDown}
        className="flex max-h-[92%] w-[min(92%,42rem)] flex-col rounded-xl border border-border bg-surface p-[0.9rem]"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-[0.7rem] flex items-center justify-between">
          {shown ? (
            <button
              type="button"
              onClick={() => setOpenPanel(null)}
              className="touch-target flex items-center gap-[0.3rem] rounded-md px-[0.4rem] text-surface-foreground hover:bg-muted"
            >
              <ArrowLeft className="h-[1.2rem] w-[1.2rem]" aria-hidden />
              <span id="quick-menu-title" className="text-[1.05rem] font-semibold">
                {shown.p.title}
              </span>
            </button>
          ) : (
            <h2 id="quick-menu-title" className="text-[1.05rem] font-semibold text-surface-foreground">
              Go to
            </h2>
          )}
          <button
            type="button"
            onClick={closeQuickMenu}
            aria-label="Close"
            className="touch-target flex items-center justify-center rounded-md px-[0.4rem] text-muted-foreground hover:bg-muted"
          >
            <X className="h-[1.3rem] w-[1.3rem]" aria-hidden />
          </button>
        </div>

        {shown ? (
          <PluginFrameHost ext={shown.ext} title={shown.p.title} className="h-[22rem] w-full rounded-lg" />
        ) : (
          <div className="overflow-y-auto">
            <div className="grid grid-cols-3 gap-[0.5rem] portrait:grid-cols-2">
              {tabs.map((tab) => {
                const Icon = tab.icon;
                const active = tab.id === activeTabId;
                return (
                  <button
                    key={tab.id}
                    type="button"
                    onClick={() => goTab(tab.id)}
                    aria-current={active ? "page" : undefined}
                    className={cn(
                      "touch-target flex min-h-[4.5rem] flex-col items-center justify-center gap-[0.3rem] rounded-md",
                      active ? "bg-primary text-primary-foreground" : "bg-muted text-surface-foreground hover:bg-border",
                    )}
                  >
                    {Icon ? <Icon className="h-[1.6rem] w-[1.6rem]" aria-hidden /> : null}
                    <span className="text-[0.85rem] font-medium">{tab.title}</span>
                  </button>
                );
              })}
            </div>

            {panels.length > 0 ? (
              <>
                <h3 className="mb-[0.4rem] mt-[0.8rem] text-[0.8rem] uppercase tracking-wide text-muted-foreground">
                  Extension panels
                </h3>
                <div className="grid grid-cols-3 gap-[0.5rem] portrait:grid-cols-2">
                  {panels.map(({ ext, p }) => (
                    <button
                      key={`${ext.pluginId}:${p.id}`}
                      type="button"
                      onClick={() => setOpenPanel(`${ext.pluginId}:${p.id}`)}
                      className="touch-target min-h-[3.5rem] rounded-md bg-muted px-[0.5rem] text-[0.85rem] text-surface-foreground hover:bg-border"
                    >
                      {p.title}
                    </button>
                  ))}
                </div>
              </>
            ) : null}

            {!kiosk ? (
              <button
                type="button"
                onClick={() => window.location.assign("/")}
                className="touch-target mt-[0.8rem] flex w-full items-center justify-center gap-[0.5rem] rounded-md border border-border text-[0.9rem] text-surface-foreground hover:bg-muted"
              >
                <LayoutDashboard className="h-[1.2rem] w-[1.2rem]" aria-hidden />
                Exit to dashboard
              </button>
            ) : null}
          </div>
        )}
      </div>
    </div>
  );
}
