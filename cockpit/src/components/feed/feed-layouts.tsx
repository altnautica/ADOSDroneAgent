// The Feed's three arrangements, mirroring the Android cockpit layouts:
//   compact  — video full-bleed, the minimap as an inset under the safety
//              band with the stream tabs beside it, radar above the Skill Bar
//   split    — video and map side by side behind a draggable divider
//   portrait — a 16:9 video under the band, tabs and alerts, the map below
// Every measure comes from lib/cockpit-layout and the shell chrome variables
// in styles/globals.css; every layer sits on the CS-1 z-ladder.

import { useRef, useState, type ReactNode } from "react";

import { ExtensionWidgetZones } from "@/components/plugins/extension-feed-mounts";
import {
  CHROME,
  INSET_ASPECT,
  INSET_WIDTH,
  SKILL_BAR_CLEARANCE_PX,
  SPLIT_MAX,
  SPLIT_MIN,
  Z,
  clampSplit,
} from "@/lib/cockpit-layout";
import { cn } from "@/lib/utils";
import { useSettingsStore } from "@/stores/settings-store";

/** What each layout places. Map and radar are absent on a node that does not
 *  fly. */
export interface FeedSlots {
  /** Video, host targets and extension overlays; each fills its parent. */
  video: ReactNode;
  hud: ReactNode;
  band: ReactNode;
  alerts: ReactNode;
  tabs: ReactNode;
  map: ((className: string) => ReactNode) | null;
  radar: ((className?: string) => ReactNode) | null;
  /** The picture-in-picture inset (a second leg), or null with one leg. */
  pip: ReactNode;
  /** The Skill Bar and the Feed's own action buttons. */
  skillRow: ReactNode;
  menuCollapsed: boolean;
}

/** The region the floating shell chrome leaves free. */
function regionClass(menuCollapsed: boolean): string {
  return cn(
    "pointer-events-none absolute flex flex-col gap-[0.3rem]",
    CHROME.regionTop,
    CHROME.regionBottom,
    CHROME.regionRight,
    menuCollapsed ? CHROME.regionLeftCollapsed : CHROME.regionLeftBesideRail,
  );
}

function Band({ children }: { children: ReactNode }) {
  return <div className={cn("relative shrink-0", Z.band)}>{children}</div>;
}

function Alerts({ children }: { children: ReactNode }) {
  return (
    <div className={cn("absolute inset-x-0 top-0 flex flex-col items-center gap-[0.3rem]", Z.alerts)}>{children}</div>
  );
}

function SkillRow({ children }: { children: ReactNode }) {
  return (
    <div className={cn("absolute inset-x-0 bottom-0 flex items-end justify-between gap-[0.6rem]", Z.skillBar)}>
      {children}
    </div>
  );
}

function Radar({ children }: { children: ReactNode }) {
  return (
    <div className={cn("absolute right-0", Z.zones)} style={{ bottom: SKILL_BAR_CLEARANCE_PX }}>
      {children}
    </div>
  );
}

/** The picture-in-picture inset, bottom-left above the Skill Bar (the radar
 *  holds bottom-right). */
function Pip({ children, bottom = SKILL_BAR_CLEARANCE_PX }: { children: ReactNode; bottom?: number }) {
  return (
    <div className={cn("absolute left-0", Z.pip)} style={{ bottom }}>
      {children}
    </div>
  );
}

export function CompactLayout({ slots }: { slots: FeedSlots }) {
  return (
    <>
      {slots.video}
      <div className={regionClass(slots.menuCollapsed)}>
        <Band>{slots.band}</Band>
        <div className="relative min-h-0 flex-1">
          {slots.hud}
          {slots.map ? (
            <div
              className={cn("absolute left-0 top-0", Z.zones)}
              style={{ width: INSET_WIDTH, aspectRatio: String(INSET_ASPECT) }}
            >
              {slots.map("h-full w-full")}
            </div>
          ) : null}
          {slots.tabs ? (
            <div
              className={cn("pointer-events-auto absolute top-0", Z.zones)}
              style={{ left: slots.map ? `calc(${INSET_WIDTH} + 0.4rem)` : 0 }}
            >
              {slots.tabs}
            </div>
          ) : null}
          <Alerts>{slots.alerts}</Alerts>
          <ExtensionWidgetZones />
          {slots.radar ? <Radar>{slots.radar()}</Radar> : null}
          {slots.pip ? <Pip>{slots.pip}</Pip> : null}
          <SkillRow>{slots.skillRow}</SkillRow>
        </div>
      </div>
    </>
  );
}

/** How far one arrow-key press moves the divider. */
const DIVIDER_KEY_STEP = 0.05;

export function SplitLayout({ slots, dividerLabel = "Video and map divider" }: { slots: FeedSlots; dividerLabel?: string }) {
  const stored = useSettingsStore((s) => s.splitRatio);
  const persist = useSettingsStore((s) => s.setSplitRatio);
  const [drag, setDrag] = useState<number | null>(null);
  const rowRef = useRef<HTMLDivElement>(null);
  const ratio = drag ?? stored;

  const ratioAt = (clientX: number) => {
    const rect = rowRef.current?.getBoundingClientRect();
    return rect && rect.width > 0 ? clampSplit((clientX - rect.left) / rect.width) : ratio;
  };

  return (
    <div className={regionClass(slots.menuCollapsed)}>
      <Band>{slots.band}</Band>
      <div ref={rowRef} className="flex min-h-0 flex-1">
        <div className="pointer-events-auto relative min-w-0 overflow-hidden rounded-lg" style={{ flex: `${ratio} 1 0` }}>
          {slots.video}
          {slots.hud}
          {slots.tabs ? <div className={cn("absolute left-0 top-0", Z.zones)}>{slots.tabs}</div> : null}
          <Alerts>{slots.alerts}</Alerts>
          <ExtensionWidgetZones topClearancePx={48} />
          {slots.radar ? <Radar>{slots.radar()}</Radar> : null}
          {slots.pip ? <Pip>{slots.pip}</Pip> : null}
          <SkillRow>{slots.skillRow}</SkillRow>
        </div>
        <div
          role="separator"
          tabIndex={0}
          aria-orientation="vertical"
          aria-label={dividerLabel}
          aria-valuemin={SPLIT_MIN * 100}
          aria-valuemax={SPLIT_MAX * 100}
          aria-valuenow={Math.round(ratio * 100)}
          className="pointer-events-auto flex w-[12px] shrink-0 cursor-col-resize touch-none items-center justify-center"
          onPointerDown={(e) => {
            e.currentTarget.setPointerCapture?.(e.pointerId);
            setDrag(ratioAt(e.clientX));
          }}
          onPointerMove={(e) => {
            if (drag !== null) setDrag(ratioAt(e.clientX));
          }}
          onPointerUp={() => {
            if (drag !== null) persist(drag);
            setDrag(null);
          }}
          onPointerCancel={() => setDrag(null)}
          onKeyDown={(e) => {
            if (e.key === "ArrowLeft") persist(ratio - DIVIDER_KEY_STEP);
            else if (e.key === "ArrowRight") persist(ratio + DIVIDER_KEY_STEP);
          }}
        >
          <span className="h-[40px] w-[4px] rounded-sm bg-border" aria-hidden />
        </div>
        <div className="relative min-w-0 overflow-hidden rounded-lg" style={{ flex: `${1 - ratio} 1 0` }}>
          {slots.map?.("h-full w-full")}
        </div>
      </div>
    </div>
  );
}

export function PortraitLayout({ slots }: { slots: FeedSlots }) {
  return (
    <div className={regionClass(slots.menuCollapsed)}>
      <Band>{slots.band}</Band>
      <div className="pointer-events-auto relative aspect-video w-full shrink-0 overflow-hidden rounded-lg">
        {slots.video}
        {slots.hud}
        {slots.pip ? <Pip bottom={6}>{slots.pip}</Pip> : null}
      </div>
      {slots.tabs ? <div className="pointer-events-auto shrink-0">{slots.tabs}</div> : null}
      <div className="flex shrink-0 flex-col items-center gap-[0.3rem]">{slots.alerts}</div>
      <div className="relative min-h-0 flex-1">
        {slots.map?.("h-full w-full")}
        {slots.radar ? (
          <div className={cn("absolute bottom-[0.4rem] right-[0.4rem]", Z.zones)}>{slots.radar("h-[96px] w-[96px]")}</div>
        ) : null}
      </div>
      <div className={cn("relative flex shrink-0 items-end justify-between gap-[0.6rem]", Z.skillBar)}>{slots.skillRow}</div>
    </div>
  );
}
