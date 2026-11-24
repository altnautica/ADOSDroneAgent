// L3 — the persistent menu. A left rail in landscape, a bottom strip in
// portrait (the panel aspect decides, via Tailwind orientation variants). The
// active tab is amber; the button/gamepad focus ring highlights the focused
// entry so the menu is fully operable eyes-on-panel with any input source.
//
// Three states on one toggle at the rail's end (and the `[` key), remembered
// per device (settings store `rail`):
// - labelled: icon and label;
// - compact: icon only, the label as a tooltip on hover, focus or a 500 ms
//   long press;
// - hidden: no rail, only a slim tab at the screen's left edge that brings it
//   back.
// Every entry stays at least 48 px in every state. When the entries do not
// fit (ten ground-station tabs on a 1280×800 kiosk) the rail scrolls, and a
// fade with a chevron at the clipped end says there is more.

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

/** A press held this long on a compact entry shows its label instead of
 *  opening it. */
const LONG_PRESS_MS = 500;
/** How long a long-press label stays up after the finger lifts. */
const TIP_HOLD_MS = 1500;

interface Tip {
  label: string;
  left: number;
  top: number;
  /** Above the entry (the portrait strip) rather than beside it. */
  above: boolean;
}

function tipFor(el: HTMLElement, label: string): Tip {
  const r = el.getBoundingClientRect();
  const above = window.innerHeight > window.innerWidth;
  return above
    ? { label, left: r.left + r.width / 2, top: r.top - 6, above }
    : { label, left: r.right + 6, top: r.top + r.height / 2, above };
}

/** The fade and chevron at a clipped end of the rail. */
function MoreHint({ axis, end, label }: { axis: "x" | "y"; end: "before" | "after"; label: string }) {
  const Icon = axis === "y" ? (end === "after" ? ChevronDown : ChevronUp) : end === "after" ? ChevronRight : ChevronLeft;
  return (
    <div
      role="img"
      aria-label={label}
      className={cn(
        "pointer-events-none absolute flex items-center justify-center text-muted-foreground",
        axis === "y" ? "inset-x-0 h-[1.6rem]" : "inset-y-0 w-[1.6rem]",
        axis === "y" && end === "after" && "bottom-0 bg-gradient-to-t from-surface to-transparent",
        axis === "y" && end === "before" && "top-0 bg-gradient-to-b from-surface to-transparent",
        axis === "x" && end === "after" && "right-0 bg-gradient-to-l from-surface to-transparent",
        axis === "x" && end === "before" && "left-0 bg-gradient-to-r from-surface to-transparent",
      )}
      data-testid={`rail-more-${end}`}
    >
      <Icon className="h-[1rem] w-[1rem]" aria-hidden />
    </div>
  );
}

/** The hidden rail's edge tab: a 12 × 64 px bar at the left edge, centred,
 *  inside a 48 px wide hit area. */
function RailEdgeTab({ onShow }: { onShow: () => void }) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      onClick={onShow}
      aria-label={t("rail.show")}
      className="fixed left-0 top-1/2 z-[46] flex h-[64px] w-[48px] -translate-y-1/2 items-center"
      data-testid="rail-edge-tab"
    >
      <span className="h-[64px] w-[12px] rounded-r-md border border-l-0 border-border bg-surface/90" aria-hidden />
    </button>
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
  const [tip, setTip] = useState<Tip | null>(null);
  const press = useRef<{ timer: number | undefined; long: boolean }>({ timer: undefined, long: false });
  const compact = rail === "compact";

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

  // The long-press timer never outlives the rail.
  useEffect(() => {
    const ref = press.current;
    return () => clearTimeout(ref.timer);
  }, []);

  if (rail === "hidden") return <RailEdgeTab onShow={cycleRail} />;

  const endPress = () => {
    clearTimeout(press.current.timer);
    if (press.current.long) press.current.timer = window.setTimeout(() => setTip(null), TIP_HOLD_MS);
  };

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
      <div className="relative flex min-h-0 min-w-0 flex-1">
        <nav
          ref={navRef}
          onScroll={measure}
          className="flex min-h-0 min-w-0 flex-1 gap-[0.2rem] overflow-auto p-[0.25rem] [scrollbar-width:none] landscape:flex-col portrait:flex-row"
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
                aria-label={compact ? tab.title : undefined}
                onClick={(e) => {
                  if (press.current.long) {
                    // A long press showed the label; it does not open the tab.
                    press.current.long = false;
                    e.preventDefault();
                    return;
                  }
                  setMenuFocus(index);
                  goTab(tab.id);
                }}
                onPointerEnter={(e) => {
                  if (compact && e.pointerType === "mouse") setTip(tipFor(e.currentTarget, tab.title));
                }}
                onPointerLeave={(e) => {
                  if (e.pointerType === "mouse") setTip(null);
                }}
                onPointerDown={(e) => {
                  press.current.long = false;
                  clearTimeout(press.current.timer);
                  if (!compact || e.pointerType === "mouse") return;
                  const el = e.currentTarget;
                  press.current.timer = window.setTimeout(() => {
                    press.current.long = true;
                    setTip(tipFor(el, tab.title));
                  }, LONG_PRESS_MS);
                }}
                onPointerUp={endPress}
                onPointerCancel={endPress}
                onFocus={(e) => {
                  if (compact) setTip(tipFor(e.currentTarget, tab.title));
                }}
                onBlur={() => setTip(null)}
                onContextMenu={(e) => {
                  if (compact) e.preventDefault();
                }}
                className={cn(
                  "flex flex-[1_0_auto] flex-col items-center justify-center gap-[0.15rem] rounded-md px-[0.25rem] py-[0.25rem] transition-colors",
                  "min-h-[calc(48px*var(--touch-scale))] min-w-[calc(48px*var(--touch-scale))]",
                  active ? "bg-primary text-primary-foreground" : "text-muted-foreground hover:bg-muted hover:text-surface-foreground",
                )}
                data-testid="rail-item"
              >
                {Icon ? <Icon className="h-[1.3rem] w-[1.3rem] shrink-0" aria-hidden /> : null}
                {compact ? null : <span className="max-w-full truncate text-[0.72rem] font-medium leading-none">{tab.title}</span>}
              </button>
            );
          })}
        </nav>
        {more.axis && more.before ? <MoreHint axis={more.axis} end="before" label={t("rail.moreAbove")} /> : null}
        {more.axis && more.after ? <MoreHint axis={more.axis} end="after" label={t("rail.moreBelow")} /> : null}
      </div>
      <button
        type="button"
        onClick={cycleRail}
        aria-label={rail === "labelled" ? t("rail.toCompact") : t("rail.toHidden")}
        className="flex min-h-[calc(48px*var(--touch-scale))] min-w-[calc(48px*var(--touch-scale))] shrink-0 items-center justify-center text-muted-foreground hover:bg-muted hover:text-surface-foreground landscape:border-t portrait:border-l border-border"
        data-testid="rail-toggle"
      >
        <ChevronsLeft className="h-[1.2rem] w-[1.2rem]" aria-hidden />
      </button>
      {tip ? (
        <div
          role="tooltip"
          className="pointer-events-none fixed z-[60] whitespace-nowrap rounded-md border border-border bg-surface px-[0.5rem] py-[0.25rem] text-[0.8rem] text-surface-foreground shadow-lg"
          style={{
            left: tip.left,
            top: tip.top,
            transform: tip.above ? "translate(-50%, -100%)" : "translateY(-50%)",
          }}
        >
          {tip.label}
        </div>
      ) : null}
    </div>
  );
}
