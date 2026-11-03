// The frame of a full console screen: a 48px header (back, title, tabs,
// actions), the scrolling body, and the controller hints along the foot. Tabs
// sit inline after the title in landscape and in a row under the header in
// portrait; the cycle command steps them, looping. Mirrors the Android
// ConsoleScaffold.
//
// Controller reach: with the menu ring on this screen's tab, select enters the
// screen (focus moves to its first control); the d-pad then moves between its
// controls, select clicks the focused one, and back leaves for the menu again
// (or runs `onBack` on a drilled screen).

import { useEffect, useRef, type ReactNode } from "react";
import { ArrowLeft } from "lucide-react";

import { HintBar, type Hint } from "@/components/ui/hint-bar";
import { pushCommandHandler } from "@/lib/command-stack";
import { activateFocused, firstFocusable, holdsFocus, moveFocus, trackFocusRing } from "@/lib/focus-nav";
import { cn } from "@/lib/utils";
import { useNavStore } from "@/stores/nav-store";

export interface ConsoleScaffoldProps {
  title: ReactNode;
  /** Shows the back button; also what the back command runs inside. */
  onBack?: () => void;
  backLabel?: string;
  tabs?: string[];
  selectedTab?: number;
  onSelectTab?: (index: number) => void;
  actions?: ReactNode;
  hints?: Hint[];
  children: ReactNode;
}

export function ConsoleScaffold({
  title,
  onBack,
  backLabel = "Back",
  tabs = [],
  selectedTab = 0,
  onSelectTab,
  actions,
  hints = [],
  children,
}: ConsoleScaffoldProps) {
  const rootRef = useRef<HTMLDivElement>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  const latest = useRef({ onBack, tabs, selectedTab, onSelectTab });
  useEffect(() => {
    latest.current = { onBack, tabs, selectedTab, onSelectTab };
  });

  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    const stopRing = trackFocusRing(root);
    const off = pushCommandHandler((cmd) => {
      const s = latest.current;
      if (cmd === "cycle-tab" && s.tabs.length > 1 && s.onSelectTab) {
        s.onSelectTab((s.selectedTab + 1) % s.tabs.length);
        return true;
      }
      if (!holdsFocus(root)) {
        // Select on this screen's own menu entry steps into the screen.
        const nav = useNavStore.getState();
        const onOwnEntry = nav.detailStack.length > 0 || nav.visibleTabs[nav.menuFocusIndex] === nav.activeTabId;
        if (cmd === "activate" && onOwnEntry) {
          const first = firstFocusable(bodyRef.current) ?? firstFocusable(root);
          if (first) {
            first.focus();
            return true;
          }
        }
        return false;
      }
      switch (cmd) {
        case "prev":
          moveFocus(root, -1);
          return true;
        case "next":
          moveFocus(root, 1);
          return true;
        case "activate":
          return activateFocused(root);
        case "back":
          if (s.onBack) s.onBack();
          else (document.activeElement as HTMLElement | null)?.blur();
          return true;
        default:
          return false;
      }
    });
    return () => {
      off();
      stopRing();
    };
  }, []);

  const tabPills =
    tabs.length > 0 ? (
      <div role="tablist" className="flex min-w-0 items-center gap-[0.25rem] overflow-x-auto">
        {tabs.map((label, i) => (
          <button
            key={label}
            type="button"
            role="tab"
            aria-selected={i === selectedTab}
            onClick={() => onSelectTab?.(i)}
            className={cn(
              "flex min-h-[max(2.25rem,36px)] shrink-0 items-center rounded-xl px-[0.75rem] text-[0.85rem] font-medium",
              i === selectedTab ? "bg-primary/15 text-primary" : "text-muted-foreground hover:bg-muted",
            )}
          >
            {label}
          </button>
        ))}
      </div>
    ) : null;

  return (
    <div ref={rootRef} className="flex h-full w-full flex-col overflow-hidden rounded-lg bg-surface/60">
      <div className="flex min-h-[max(2.75rem,48px)] shrink-0 items-center gap-[0.5rem] px-[0.5rem]">
        {onBack ? (
          <button
            type="button"
            onClick={onBack}
            aria-label={backLabel}
            className="touch-target flex items-center justify-center rounded-full text-surface-foreground hover:bg-muted"
          >
            <ArrowLeft className="h-[1.3rem] w-[1.3rem]" aria-hidden />
          </button>
        ) : null}
        <h1 className={cn("truncate text-[1.1rem] font-semibold tracking-tight text-surface-foreground", !onBack && "pl-[0.4rem]")}>
          {title}
        </h1>
        <div className="hidden min-w-0 flex-1 landscape:flex">{tabPills}</div>
        <div className="flex-1 landscape:hidden" />
        {actions ? <div className="flex shrink-0 items-center gap-[0.25rem]">{actions}</div> : null}
      </div>
      {tabPills ? <div className="shrink-0 px-[0.5rem] pb-[0.3rem] landscape:hidden">{tabPills}</div> : null}
      <div ref={bodyRef} className="min-h-0 flex-1 overflow-y-auto p-[0.6rem]">
        {children}
      </div>
      <HintBar hints={hints} />
    </div>
  );
}
