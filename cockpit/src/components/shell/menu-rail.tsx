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
//   back. In landscape the tab sits in a 48 px slot of its own beside the
//   screen, so its hit area never covers the Feed's flight-action column.
// Every entry stays at least 48 px in every state, and the labelled entries
// are sized in bounded px (about 52 px tall) so a drone's eleven tabs fit a
// 1280×800 kiosk without scrolling. When they still do not fit (glove mode)
// the rail scrolls, and a chevron strip at the clipped end, in the flow
// rather than over the entries, says there is more.

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

/** The chevron strip at a clipped end of the rail: in the flow beside the
 *  entries, so it never covers a label. */
function MoreHint({ axis, end, label }: { axis: "x" | "y"; end: "before" | "after"; label: string }) {
  const Icon = axis === "y" ? (end === "after" ? ChevronDown : ChevronUp) : end === "after" ? ChevronRight : ChevronLeft;
  return (
    <div
      role="img"
      aria-label={label}
      className={cn("pointer-events-none flex shrink-0 items-center justify-center text-muted-foreground", axis === "y" ? "h-[18px] w-full" : "h-full w-[18px]")}
      data-testid={`rail-more-${end}`}
    >
      <Icon className="h-[14px] w-[14px]" aria-hidden />
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
      <div className="relative flex min-h-0 min-w-0 flex-1 landscape:flex-col portrait:flex-row">
        {more.axis && more.before ? <MoreHint axis={more.axis} end="before" label={t("rail.moreAbove")} /> : null}
        <nav
          ref={navRef}
          onScroll={measure}
          className="flex min-h-0 min-w-0 flex-1 gap-[2px] overflow-auto p-[4px] [scrollbar-width:none] landscape:flex-col portrait:flex-row"
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
                  "flex shrink-0 flex-col items-center justify-center gap-[2px] rounded-md px-[4px] py-[4px] transition-colors portrait:flex-[1_0_auto]",
                  "min-h-[calc(48px*var(--touch-scale))] min-w-[calc(48px*var(--touch-scale))]",
                  active ? "bg-primary text-primary-foreground" : "text-muted-foreground hover:bg-muted hover:text-surface-foreground",
                )}
                data-testid="rail-item"
              >
                {Icon ? <Icon className="h-[clamp(20px,1rem,26px)] w-[clamp(20px,1rem,26px)] shrink-0" aria-hidden /> : null}
                {compact ? null : (
                  <span className="max-w-full truncate text-[length:clamp(11px,0.5rem,14px)] font-medium leading-[1.15]">{tab.title}</span>
                )}
              </button>
            );
          })}
        </nav>
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
