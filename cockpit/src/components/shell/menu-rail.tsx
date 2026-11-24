// L3 — the persistent menu. A left rail in landscape, a bottom strip in
// portrait (the panel aspect decides, via Tailwind orientation variants). The
// active tab is amber; the button/gamepad focus ring highlights the focused
// entry so the menu is fully operable eyes-on-panel with any input source.
//
// Two states on one toggle at the rail's end (and the `[` key), remembered
// per device (settings store `rail`):
// - labelled: a narrow column, each entry a short label under its icon (the
//   full page name stays the entry's accessible name and tooltip);
// - hidden: no rail, only a slim tab at the screen's left edge that brings it
//   back. In landscape the tab sits in a 48 px slot of its own beside the
//   screen, so its hit area never covers the Feed's flight-action column.
// The rail, icons and labels are sized in rem, so they keep the same
// proportion to the screen beside them at every size (the root is bounded,
// lib/ui-scale.ts). Every entry stays at least 48 px tall, so a drone's
// eleven tabs fit a 1280×800 kiosk without scrolling. When they still do not
// fit (glove mode, a short window) the rail scrolls, and a chevron strip at
// the clipped end, in the flow rather than over the entries, says there is
// more.

import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp, ChevronsLeft } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { useNodeIdentity } from "@/shared/use-profile";
import { useNavStore } from "@/stores/nav-store";
import { useSettingsStore } from "@/stores/settings-store";
import { useStatusStore } from "@/stores/status-store";
import { tabScreens } from "@/nav/registry";
import { CHROME } from "@/lib/cockpit-layout";
import { cn } from "@/lib/utils";

import { NO_OVERFLOW, railOverflow, type RailOverflow } from "./rail-overflow";

/** The chevron strip at a clipped end of the rail: in the flow beside the
 *  entries, so it never covers a label. */
function MoreHint({ axis, end, label }: { axis: "x" | "y"; end: "before" | "after"; label: string }) {
  const Icon = axis === "y" ? (end === "after" ? ChevronDown : ChevronUp) : end === "after" ? ChevronRight : ChevronLeft;
  return (
    <div
      role="img"
      aria-label={label}
      className={cn("pointer-events-none flex shrink-0 items-center justify-center text-muted-foreground", axis === "y" ? "h-[1.125rem] w-full" : "h-full w-[1.125rem]")}
      data-testid={`rail-more-${end}`}
    >
      <Icon className="h-[0.875rem] w-[0.875rem]" aria-hidden />
    </div>
  );
}

/** The hidden rail's edge tab: a 20 × 64 px tab at the left edge, centred,
 *  filled in the primary colour with a chevron so it reads on dark video,
 *  inside a 48 px wide hit area. In landscape that hit area has a 48 px
 *  column of its own (the slot), beside the screen rather than over it. */
export function RailEdgeTab({ onShow }: { onShow: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="relative shrink-0 landscape:w-[48px] landscape:self-stretch portrait:h-0 portrait:w-0" data-testid="rail-edge-slot">
      <button
        type="button"
        onClick={onShow}
        aria-label={t("rail.show")}
        className="absolute left-0 top-1/2 z-[46] flex h-[64px] w-[48px] -translate-y-1/2 items-center portrait:fixed"
        data-testid="rail-edge-tab"
      >
        <span
          className="flex h-[64px] w-[20px] items-center justify-center rounded-r-md border border-l-0 border-primary bg-primary text-primary-foreground shadow-md"
          data-testid="rail-edge-tab-fill"
          aria-hidden
        >
          <ChevronRight className="h-[16px] w-[16px]" strokeWidth={3} />
        </span>
      </button>
    </div>
  );
}

export function MenuRail() {
  const activeTabId = useNavStore((s) => s.activeTabId);
  const menuFocusIndex = useNavStore((s) => s.menuFocusIndex);
  const goTab = useNavStore((s) => s.goTab);
  const setMenuFocus = useNavStore((s) => s.setMenuFocus);
  const rail = useSettingsStore((s) => s.rail);
  const cycleRail = useSettingsStore((s) => s.cycleRail);
  const { profile, role } = useNodeIdentity();
  const fcFamily = useStatusStore((s) => s.status?.fc_family ?? null);
  const tabs = tabScreens(profile, role, fcFamily);
  const { t } = useTranslation();
  const navRef = useRef<HTMLElement>(null);
  const [more, setMore] = useState<RailOverflow>(NO_OVERFLOW);

  const measure = useCallback(() => {
    const n = navRef.current;
    if (!n) return;
    const next = railOverflow(n);
    setMore((cur) => (cur.axis === next.axis && cur.before === next.before && cur.after === next.after ? cur : next));
  }, []);

  useLayoutEffect(() => {
    const n = navRef.current;
    if (!n) return;
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(n);
    return () => ro.disconnect();
  }, [measure, rail, tabs.length]);

  // Keep the focused (or active) entry in view: on a tall menu the ring can
  // otherwise move below the fold where the operator cannot see it.
  useEffect(() => {
    const el = navRef.current?.querySelector<HTMLElement>("[data-focused]") ?? navRef.current?.querySelector<HTMLElement>("[aria-current]");
    el?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [menuFocusIndex, activeTabId]);

  if (rail === "hidden") return <RailEdgeTab onShow={cycleRail} />;

  return (
    <div
      data-rail={rail}
      className={cn(
        "relative flex shrink-0",
        // rail (landscape) vs strip (portrait)
        "landscape:flex-col portrait:w-full portrait:flex-row",
        CHROME.menuSize,
        "landscape:border-r landscape:border-border portrait:border-t portrait:border-border bg-surface/50",
      )}
      data-testid="menu-rail"
    >
      <div className="relative flex min-h-0 min-w-0 flex-1 landscape:flex-col portrait:flex-row">
        {more.axis && more.before ? <MoreHint axis={more.axis} end="before" label={t("rail.moreAbove")} /> : null}
        <nav
          ref={navRef}
          onScroll={measure}
          className="flex min-h-0 min-w-0 flex-1 gap-[0.125rem] overflow-auto p-[0.25rem] [scrollbar-width:none] landscape:flex-col portrait:flex-row"
          aria-label={t("cockpit.shell.menu")}
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
                aria-label={tab.title}
                title={tab.title}
                onClick={() => {
                  setMenuFocus(index);
                  goTab(tab.id);
                }}
                className={cn(
                  "flex shrink-0 flex-col items-center justify-center gap-[0.2rem] rounded-md px-[0.125rem] py-[0.3rem] transition-colors portrait:flex-[1_0_auto] portrait:px-[0.4rem]",
                  "min-h-[calc(48px*var(--touch-scale))] min-w-[calc(48px*var(--touch-scale))]",
                  active ? "bg-primary text-primary-foreground" : "text-muted-foreground hover:bg-muted hover:text-surface-foreground",
                )}
                data-testid="rail-item"
              >
                {Icon ? <Icon className="h-[1.375rem] w-[1.375rem] shrink-0" aria-hidden /> : null}
                <span className="whitespace-nowrap text-[0.6875rem] font-medium leading-tight" data-testid="rail-label" aria-hidden>
                  {t(`rail.label.${tab.id}`, { defaultValue: tab.title })}
                </span>
              </button>
            );
          })}
        </nav>
        {more.axis && more.after ? <MoreHint axis={more.axis} end="after" label={t("rail.moreBelow")} /> : null}
      </div>
      <button
        type="button"
        onClick={cycleRail}
        aria-label={t("rail.hide")}
        title={t("rail.hide")}
        className="flex min-h-[calc(48px*var(--touch-scale))] min-w-[calc(48px*var(--touch-scale))] shrink-0 items-center justify-center text-muted-foreground hover:bg-muted hover:text-surface-foreground landscape:border-t portrait:border-l border-border"
        data-testid="rail-toggle"
      >
        <ChevronsLeft className="h-[1.25rem] w-[1.25rem]" aria-hidden />
      </button>
    </div>
  );
}
