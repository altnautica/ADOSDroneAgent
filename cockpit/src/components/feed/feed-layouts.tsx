// The Feed in the DJI grid (cockpit spec `layout.json`), per view (`views.json`):
//   pilot  — video main; the grid floats over it
//   map    — map main, the video as a picture-in-picture
//   split  — video and map side by side behind a draggable divider
//   vision — video main with the vision side panel
// Landscape: the top bar across the top, the left column (Take off / Land,
// RTH, Pause) and the right column (camera) vertically centred, the
// bottom-left card (map / attitude / obstacle radar; a 96 px chip on compact),
// the telemetry row bottom-centre and the skill picker bottom-right.
// Portrait stacks a 16:9 video with the top bar inside it over the left
// column as a row, the skill picker, the telemetry row and the map. Every
// layer sits on the CS-1 z-ladder; a television scales the chrome by 1.33.

import { useRef, useState, type CSSProperties, type ReactNode } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";

import { ExtensionWidgetZones } from "@/components/plugins/extension-feed-mounts";
import { CHROME, SPLIT_MAX, SPLIT_MIN, Z, clampSplit, type GridMeasures } from "@/lib/cockpit-layout";
import type { CockpitView } from "@/lib/cockpit-view";
import { cn } from "@/lib/utils";
import { useCockpitUiStore, type BottomLeftCard } from "@/stores/cockpit-ui-store";
import { useSettingsStore } from "@/stores/settings-store";

/** What the grid places. Map, radar and the cards are absent on a node that
 *  does not fly. */
export interface FeedSlots {
  /** Video, host targets, data layers and extension overlays; fills its parent. */
  video: ReactNode;
  /** The instrument HUD and follow chrome over the video. */
  hud: ReactNode;
  topBar: ReactNode;
  alerts: ReactNode;
  /** Handover banner, gate ring, countdowns. */
  overlays: ReactNode;
  tabs: ReactNode;
  map: ((className: string) => ReactNode) | null;
  /** The bottom-left card's contents per card id. */
  cards: Partial<Record<BottomLeftCard, (className: string) => ReactNode>>;
  pip: ReactNode;
  leftColumn: (row: boolean) => ReactNode;
  rightColumn: ReactNode;
  bottomRow: ReactNode;
  skillPicker: ReactNode;
  /** The last command outcome line. */
  ack: ReactNode;
  visionPanel: ReactNode;
  onscreenSticks: ReactNode;
  /** The follow rail holds the right edge: the right column moves inward. */
  railPresent: boolean;
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

/** Chrome scaled for the class (×1.33 on a television). */
function Scaled({ m, className, style, children }: { m: GridMeasures; className?: string; style?: CSSProperties; children: ReactNode }) {
  return (
    <div className={className} style={{ ...style, zoom: m.scale === 1 ? undefined : m.scale }}>
      {children}
    </div>
  );
}

const CARD_LABEL: Record<BottomLeftCard, string> = {
  map: "Map",
  attitude_radar: "Attitude",
  obstacle_radar: "Obstacles",
};

/** The bottom-left card: one of map / attitude + radar / obstacle radar,
 *  switched by swipe or its arrows (L3 swaps video and map). */
function BottomLeftCardView({ slots, m }: { slots: FeedSlots; m: GridMeasures }) {
  const card = useCockpitUiStore((s) => s.card);
  const nextCard = useCockpitUiStore((s) => s.nextCard);
  const swipeX = useRef<number | null>(null);
  const available = (Object.keys(CARD_LABEL) as BottomLeftCard[]).filter((c) => slots.cards[c]);
  if (available.length === 0) return null;
  const shown = available.includes(card) ? card : available[0];
  const render = slots.cards[shown];
  const size: CSSProperties =
    m.card.kind === "chip" ? { width: m.card.sizePx, height: m.card.sizePx } : { width: m.card.widthPx, height: m.card.heightPx };
  return (
    <div
      className="pointer-events-auto relative overflow-hidden rounded-lg border border-hud-hair bg-hud-glass backdrop-blur-hud"
      style={size}
      data-testid="bottom-left-card"
      data-card={shown}
      onPointerDown={(e) => {
        swipeX.current = e.clientX;
      }}
      onPointerUp={(e) => {
        if (swipeX.current !== null && Math.abs(e.clientX - swipeX.current) > 40) nextCard(e.clientX < swipeX.current ? 1 : -1);
        swipeX.current = null;
      }}
    >
      {render?.("h-full w-full")}
      {m.card.kind === "card" && available.length > 1 ? (
        <div className="absolute inset-x-0 bottom-0 flex items-center justify-between px-[0.2rem]">
          <button type="button" aria-label="Previous card" onClick={() => nextCard(-1)} className="flex h-[1.6rem] w-[1.6rem] items-center justify-center rounded bg-hud-glass-strong/70 text-hud-ink">
            <ChevronLeft className="h-[0.9rem] w-[0.9rem]" aria-hidden />
          </button>
          <span className="rounded bg-hud-glass-strong/70 px-[0.3rem] font-mono text-[0.68rem] text-hud-ink-2">{CARD_LABEL[shown]}</span>
          <button type="button" aria-label="Next card" onClick={() => nextCard(1)} className="flex h-[1.6rem] w-[1.6rem] items-center justify-center rounded bg-hud-glass-strong/70 text-hud-ink">
            <ChevronRight className="h-[0.9rem] w-[0.9rem]" aria-hidden />
          </button>
        </div>
      ) : null}
    </div>
  );
}

/** The grid chrome over the main surface. */
function GridChrome({ slots, m, view, showCard }: { slots: FeedSlots; m: GridMeasures; view: CockpitView; showCard: boolean }) {
  // The left column centres in the height the bottom-left card leaves free.
  const cardPx = showCard ? (m.card.kind === "chip" ? m.card.sizePx : m.card.heightPx) * m.scale : 0;
  return (
    <>
      <div className={cn("pointer-events-none absolute inset-x-0 top-0 flex flex-col items-center gap-[0.3rem]", Z.alerts)}>
        {slots.alerts}
        {slots.overlays}
      </div>
      {view !== "map" ? <ExtensionWidgetZones /> : null}
      <Scaled
        m={m}
        className={cn("absolute left-[0.4rem] -translate-y-1/2", Z.skillBar)}
        style={{ top: `calc((100% - ${cardPx + 16}px) / 2)` }}
      >
        {slots.leftColumn(false)}
      </Scaled>
      <Scaled
        m={m}
        className={cn("absolute top-1/2 -translate-y-1/2", Z.skillBar)}
        style={{ right: slots.railPresent ? "calc(var(--cockpit-col) + 1.4rem)" : "0.4rem" }}
      >
        {slots.rightColumn}
      </Scaled>
      <Scaled m={m} className={cn("absolute bottom-[0.4rem] left-[0.4rem] flex flex-col items-start gap-[0.3rem]", Z.zones)}>
        {slots.ack}
        {showCard ? <BottomLeftCardView slots={slots} m={m} /> : null}
      </Scaled>
      <Scaled m={m} className={cn("absolute bottom-[0.4rem] left-1/2 -translate-x-1/2", Z.skillBar)}>
        {slots.bottomRow}
      </Scaled>
      <Scaled m={m} className={cn("absolute bottom-[0.4rem] right-[0.4rem]", Z.skillBar)}>
        {slots.skillPicker}
      </Scaled>
      {slots.onscreenSticks}
    </>
  );
}

/** How far one arrow-key press moves the divider. */
const DIVIDER_KEY_STEP = 0.05;

function Divider({ ratio, onDrag, onCommit, rowRef }: {
  ratio: number;
  onDrag: (r: number | null) => void;
  onCommit: (r: number) => void;
  rowRef: React.RefObject<HTMLDivElement>;
}) {
  const [dragging, setDragging] = useState(false);
  const ratioAt = (clientX: number) => {
    const rect = rowRef.current?.getBoundingClientRect();
    return rect && rect.width > 0 ? clampSplit((clientX - rect.left) / rect.width) : ratio;
  };
  return (
    <div
      role="separator"
      tabIndex={0}
      aria-orientation="vertical"
      aria-label="Video and map divider"
      aria-valuemin={SPLIT_MIN * 100}
      aria-valuemax={SPLIT_MAX * 100}
      aria-valuenow={Math.round(ratio * 100)}
      className="pointer-events-auto flex w-[12px] shrink-0 cursor-col-resize touch-none items-center justify-center"
      onPointerDown={(e) => {
        e.currentTarget.setPointerCapture?.(e.pointerId);
        setDragging(true);
        onDrag(ratioAt(e.clientX));
      }}
      onPointerMove={(e) => {
        if (dragging) onDrag(ratioAt(e.clientX));
      }}
      onPointerUp={(e) => {
        if (dragging) onCommit(ratioAt(e.clientX));
        setDragging(false);
        onDrag(null);
      }}
      onPointerCancel={() => {
        setDragging(false);
        onDrag(null);
      }}
      onKeyDown={(e) => {
        if (e.key === "ArrowLeft") onCommit(ratio - DIVIDER_KEY_STEP);
        else if (e.key === "ArrowRight") onCommit(ratio + DIVIDER_KEY_STEP);
      }}
    >
      <span className="h-[40px] w-[4px] rounded-sm bg-border" aria-hidden />
    </div>
  );
}

export function LandscapeGrid({ slots, m, view }: { slots: FeedSlots; m: GridMeasures; view: CockpitView }) {
  const stored = useSettingsStore((s) => s.splitRatio);
  const persist = useSettingsStore((s) => s.setSplitRatio);
  const [drag, setDrag] = useState<number | null>(null);
  const rowRef = useRef<HTMLDivElement>(null);
  const ratio = drag ?? stored;
  const mapMain = view === "map" && slots.map !== null;

  return (
    <div className={regionClass(slots.menuCollapsed)}>
      <Scaled m={m} className={cn("relative shrink-0", Z.band)}>
        {slots.topBar}
      </Scaled>
      <div ref={rowRef} className="flex min-h-0 flex-1 gap-0">
        <div className="pointer-events-auto relative min-w-0 overflow-hidden rounded-lg" style={{ flex: view === "split" ? `${ratio} 1 0` : "1 1 0" }}>
          {mapMain ? (
            <>
              {slots.map?.("h-full w-full")}
              <div className={cn("absolute right-[0.4rem] top-[0.4rem] aspect-video w-[clamp(160px,24%,320px)] overflow-hidden rounded-lg border border-hud-hair", Z.pip)} data-testid="video-pip">
                {slots.video}
              </div>
            </>
          ) : (
            <>
              {slots.video}
              {slots.hud}
              {slots.pip ? <div className={cn("absolute right-[calc(var(--cockpit-col)+1rem)] top-[0.4rem]", Z.pip)}>{slots.pip}</div> : null}
            </>
          )}
          {slots.tabs ? <div className={cn("pointer-events-auto absolute left-[calc(var(--cockpit-col)+1rem)] top-[0.4rem]", Z.zones)}>{slots.tabs}</div> : null}
          <GridChrome slots={slots} m={m} view={view} showCard={view === "pilot" || view === "vision"} />
        </div>
        {view === "split" && slots.map ? (
          <>
            <Divider ratio={ratio} rowRef={rowRef} onDrag={setDrag} onCommit={(r) => persist(r)} />
            <div className="pointer-events-auto relative min-w-0 overflow-hidden rounded-lg" style={{ flex: `${1 - ratio} 1 0` }}>
              {slots.map("h-full w-full")}
            </div>
          </>
        ) : null}
        {view === "vision" ? (
          <div className="pointer-events-auto ml-[0.4rem] w-[clamp(240px,28%,380px)] shrink-0 overflow-y-auto rounded-lg border border-hud-hair bg-hud-glass backdrop-blur-hud">
            {slots.visionPanel}
          </div>
        ) : null}
      </div>
    </div>
  );
}

export function PortraitGrid({ slots }: { slots: FeedSlots }) {
  return (
    <div className={regionClass(slots.menuCollapsed)}>
      <div className="pointer-events-auto relative aspect-video w-full shrink-0 overflow-hidden rounded-lg">
        {slots.video}
        {slots.hud}
        <div className={cn("absolute inset-x-[0.3rem] top-[0.3rem]", Z.band)}>{slots.topBar}</div>
        <div className={cn("pointer-events-none absolute inset-x-0 top-[3.4rem] flex flex-col items-center gap-[0.3rem]", Z.alerts)}>
          {slots.overlays}
        </div>
        {slots.pip ? <div className={cn("absolute bottom-[0.3rem] right-[0.3rem]", Z.pip)}>{slots.pip}</div> : null}
      </div>
      {slots.tabs ? <div className="pointer-events-auto shrink-0">{slots.tabs}</div> : null}
      <div className="flex shrink-0 flex-col items-center gap-[0.3rem]">{slots.alerts}</div>
      <div className={cn("flex shrink-0 items-center justify-between gap-[0.4rem]", Z.skillBar)}>
        {slots.leftColumn(true)}
        {slots.skillPicker}
      </div>
      <div className="flex shrink-0 justify-center">{slots.bottomRow}</div>
      {slots.ack}
      <div className="relative min-h-0 flex-1 overflow-hidden rounded-lg">
        {slots.map?.("h-full w-full")}
        {slots.cards.obstacle_radar ? (
          <div className={cn("absolute bottom-[0.4rem] right-[0.4rem] h-[96px] w-[96px]", Z.zones)}>
            {slots.cards.obstacle_radar("h-full w-full")}
          </div>
        ) : null}
      </div>
    </div>
  );
}
