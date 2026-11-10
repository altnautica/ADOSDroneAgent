// The Feed in the cockpit grid (cockpit spec `layout.json`), per view (`views.json`):
//   pilot  — video main, edge to edge; the grid floats over it
//   map    — map main, the video in the bottom-left card (`map.video_slot`)
//   split  — video and map side by side behind a draggable divider
//   vision — video main with the vision side panel
// Landscape places every zone at the rectangle `lib/flight-layout` computes
// for the viewport: the top bar across the top (STOP at its right end), the
// left and right columns vertically centred between the top bar and the
// bottom zones, the bottom-left card, the telemetry row and the skill picker
// along the bottom, and the follow strip under the top bar between the
// columns. Portrait stacks a 16:9 video with the top bar inside it over the
// left column as a row, the skill picker, the telemetry row and the map.
// The data layers draw only over the main video: the video in the card and
// a picture-in-picture inset never carry layers or targets
// (`layers.json` `pip_draws_layers: false`). Every layer sits on the CS-1
// z-ladder; a television scales the chrome by its class scale.

import { ArrowLeftRight } from "lucide-react";
import { useRef, useState, type CSSProperties, type ReactNode } from "react";

import { ExtensionWidgetZones } from "@/components/plugins/extension-feed-mounts";
import { SPLIT_MAX, SPLIT_MIN, Z, clampSplit, type GridMeasures } from "@/lib/cockpit-layout";
import type { CockpitView } from "@/lib/cockpit-view";
import { FLIGHT_PX, flightLayout, type FlightLayoutInput, type Rect, type RightItem } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitUiStore, type BottomLeftCard } from "@/stores/cockpit-ui-store";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";

/** What the grid places. Map, radar and the cards are absent on a node that
 *  does not fly. */
export interface FeedSlots {
  /** The video alone; fills its parent. */
  video: ReactNode;
  /** Host targets, data layers and extension overlays over the main video;
   *  fills its parent. */
  layers: ReactNode;
  /** The flight instruments and centre marks; fills its parent. */
  hud: ReactNode;
  topBar: ReactNode;
  alerts: ReactNode;
  /** Handover banner, gate ring, countdowns. */
  overlays: ReactNode;
  map: ((className: string) => ReactNode) | null;
  /** The bottom-left card's contents per card id. */
  cards: Partial<Record<BottomLeftCard, (className: string) => ReactNode>>;
  pip: ReactNode;
  leftColumn: (o: { row: boolean; labels: boolean }) => ReactNode;
  rightColumn: (items: readonly RightItem[]) => ReactNode;
  bottomRow: ((short: boolean) => ReactNode) | null;
  /** The follow strip's contents, given the band height under it. */
  followStrip: ((bandHeightPx: number) => ReactNode) | null;
  skillPicker: ReactNode;
  /** The last command outcome line. */
  ack: ReactNode;
  visionPanel: ReactNode;
  onscreenSticks: ReactNode;
}

/** A zone at its rectangle, its contents scaled by the class scale. CSS
 *  `zoom` scales the zone's own offsets and size too, so they are given
 *  unscaled. */
function Zone({
  r,
  scale,
  className,
  zone,
  vars,
  children,
}: {
  r: Rect;
  scale: number;
  className?: string;
  zone?: string;
  /** CSS custom properties for the zone's contents. */
  vars?: Record<`--${string}`, string>;
  children: ReactNode;
}) {
  const style: CSSProperties = {
    ...vars,
    left: r.x / scale,
    top: r.y / scale,
    width: r.w / scale,
    height: r.h / scale,
    zoom: scale === 1 ? undefined : scale,
  };
  return (
    <div className={cn("absolute", className)} style={style} data-zone-rect={zone}>
      {children}
    </div>
  );
}

const CARD_ORDER: readonly BottomLeftCard[] = ["map", "attitude_radar", "obstacle_radar"];

/** A swipe longer than this switches the card. */
const SWIPE_PX = 40;

/** The bottom-left card: one of map / attitude + radar / obstacle radar,
 *  switched by a swipe or its dots (L3 swaps video and map). Each face
 *  carries its small title inside the card. */
function BottomLeftCardView({ slots, m }: { slots: FeedSlots; m: GridMeasures }) {
  const card = useCockpitUiStore((s) => s.card);
  const nextCard = useCockpitUiStore((s) => s.nextCard);
  const swipeX = useRef<number | null>(null);
  const available = CARD_ORDER.filter((c) => slots.cards[c]);
  if (available.length === 0) return null;
  const shown = available.includes(card) ? card : available[0];
  const render = slots.cards[shown];
  const at = available.indexOf(shown);
  return (
    <div
      className="chrome-panel pointer-events-auto relative h-full w-full touch-pan-y overflow-hidden"
      data-testid="bottom-left-card"
      data-zone="bottom_left_card"
      data-card={shown}
      onPointerDown={(e) => {
        swipeX.current = e.clientX;
      }}
      onPointerUp={(e) => {
        if (swipeX.current !== null && Math.abs(e.clientX - swipeX.current) > SWIPE_PX) nextCard(e.clientX < swipeX.current ? 1 : -1);
        swipeX.current = null;
      }}
      onPointerCancel={() => {
        swipeX.current = null;
      }}
    >
      {render?.("h-full w-full")}
      {m.card.kind === "card" && available.length > 1 ? (
        // The dots: a strip the touch floor high; each dot's hit area is the
        // touch floor square, its drawn dot small.
        <div className="pointer-events-none absolute inset-x-0 bottom-0 flex justify-center" style={{ height: FLIGHT_PX.touchMin }} role="tablist">
          {available.map((c, i) => (
            <button
              key={c}
              type="button"
              role="tab"
              aria-selected={i === at}
              aria-label={specText(`cockpit.card.${c}`)}
              onClick={() => {
                const d = (i - at + available.length) % available.length;
                if (d !== 0) nextCard(d === 1 ? 1 : -1);
              }}
              className="pointer-events-auto flex items-center justify-center"
              style={{ width: FLIGHT_PX.touchMin, height: FLIGHT_PX.touchMin }}
              data-testid={`card-dot-${c}`}
            >
              <span className={cn("block h-[8px] w-[8px] rounded-full", i === at ? "bg-[var(--fs-text)]" : "bg-white/35")} aria-hidden />
            </button>
          ))}
        </div>
      ) : null}
    </div>
  );
}

/** How far one arrow-key press moves the divider. */
const DIVIDER_KEY_STEP = 0.05;

function Divider({ r, ratio, onDrag, onCommit }: {
  r: Rect;
  ratio: number;
  onDrag: (r: number | null) => void;
  onCommit: (r: number) => void;
}) {
  const [dragging, setDragging] = useState(false);
  const ratioAt = (clientX: number) => clampSplit(clientX / window.innerWidth);
  return (
    <div
      role="separator"
      tabIndex={0}
      aria-orientation="vertical"
      aria-label={specText("cockpit.divider")}
      aria-valuemin={Math.round(SPLIT_MIN * 100)}
      aria-valuemax={Math.round(SPLIT_MAX * 100)}
      aria-valuenow={Math.round(ratio * 100)}
      className={cn("pointer-events-auto absolute flex cursor-col-resize touch-none items-center justify-center", Z.zoneChrome)}
      style={{ left: r.x, top: 0, width: r.w, bottom: 0 }}
      data-testid="split-divider"
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
      <span className="h-[48px] w-[4px] rounded-full bg-white/50" aria-hidden />
    </div>
  );
}

/** The main video with its layers, zoomed by the digital zoom (the layers
 *  scale with the picture so they stay on it). Marked as the main video for
 *  the photo's frame grab. */
function MainVideo({ video, layers }: { video: ReactNode; layers: ReactNode }) {
  const zoom = useCockpitViewStore((s) => s.digitalZoom);
  return (
    <div
      className="absolute inset-0 origin-center transition-transform duration-200 ease-out"
      style={zoom === 1 ? undefined : { transform: `scale(${zoom})` }}
      data-main-video
    >
      {video}
      {layers}
    </div>
  );
}

export function LandscapeGrid({
  slots,
  m,
  input,
}: {
  slots: FeedSlots;
  m: GridMeasures;
  input: Omit<FlightLayoutInput, "splitRatio">;
}) {
  const stored = useCockpitViewStore((s) => s.split);
  const persist = useCockpitViewStore((s) => s.setSplit);
  const [drag, setDrag] = useState<number | null>(null);
  const ratio = drag ?? stored;
  const view: CockpitView = input.view === "map" && slots.map === null ? "pilot" : input.view;
  const L = flightLayout({ ...input, view, splitRatio: ratio });
  const s = m.scale;
  const mapMain = view === "map";
  const midTop = L.strip ? L.strip.y + L.strip.h + 8 * s : L.topBar.y + L.topBar.h + 8 * s;

  return (
    <div className="pointer-events-none absolute inset-0" data-flight-view={view}>
      {/* The stage: video (or the map) edge to edge. */}
      <div className="pointer-events-auto absolute overflow-hidden" style={{ left: L.stage.x, top: L.stage.y, width: L.stage.w, height: L.stage.h }} data-zone="stage">
        {mapMain ? (
          slots.map?.("h-full w-full")
        ) : (
          <>
            <MainVideo video={slots.video} layers={slots.layers} />
            {slots.pip ? (
              <div className={cn("absolute", Z.pip)} style={{ right: L.stage.w - L.rightColumn.x + 8 * s, top: midTop }}>
                {slots.pip}
              </div>
            ) : null}
          </>
        )}
      </div>
      {!mapMain ? (
        <div className={cn("pointer-events-none absolute", Z.hud)} style={{ left: L.hud.x, top: L.hud.y, width: L.hud.w, height: L.hud.h }} data-zone="hud">
          {slots.hud}
        </div>
      ) : null}
      {L.mapPane && slots.map ? (
        <div className="pointer-events-auto absolute overflow-hidden" style={{ left: L.mapPane.x, top: L.mapPane.y, width: L.mapPane.w, height: L.mapPane.h }} data-zone="map_pane">
          {slots.map("h-full w-full")}
        </div>
      ) : null}
      {L.divider ? <Divider r={L.divider} ratio={ratio} onDrag={setDrag} onCommit={(r) => persist(r)} /> : null}
      {L.visionPanel ? (
        <div
          className="chrome-panel pointer-events-auto absolute overflow-y-auto"
          style={{ left: L.visionPanel.x, top: L.visionPanel.y, width: L.visionPanel.w, height: L.visionPanel.h }}
          data-zone="vision_panel"
        >
          {slots.visionPanel}
        </div>
      ) : null}

      {view !== "map" ? <ExtensionWidgetZones /> : null}

      <Zone r={L.topBar} scale={s} className={Z.topBar} zone="top_bar">
        {slots.topBar}
      </Zone>
      {L.strip && slots.followStrip ? (
        <Zone r={L.strip} scale={s} className={Z.zoneChrome} zone="follow_strip">
          {slots.followStrip(L.bandHeight)}
        </Zone>
      ) : null}
      <div
        className={cn("pointer-events-none absolute flex flex-col items-center gap-[6px]", Z.alerts)}
        style={{ left: L.alerts.x, top: L.alerts.y, width: L.alerts.w }}
      >
        {slots.alerts}
        {slots.overlays}
        {slots.ack}
      </div>
      {L.leftColumn ? (
        <Zone
          r={L.leftColumn}
          scale={s}
          className={cn("flex justify-center", Z.zoneChrome)}
          zone="left_column"
          vars={{ "--cockpit-col": `${L.leftButtonPx}px`, "--cockpit-col-gap": `${L.leftGapPx}px` }}
        >
          {slots.leftColumn({ row: false, labels: L.leftLabels })}
        </Zone>
      ) : null}
      <Zone r={L.rightColumn} scale={s} className={cn("flex justify-center", Z.zoneChrome)} zone="right_column">
        {slots.rightColumn(L.rightItems)}
      </Zone>
      {L.card ? (
        <Zone r={L.card} scale={s} className={Z.zones} zone="bottom_left_card">
          {L.cardHoldsVideo ? (
            <div className="chrome-panel pointer-events-auto relative h-full w-full overflow-hidden" data-testid="video-card" data-zone="bottom_left_card">
              <div className="absolute inset-0" data-main-video>
                {slots.video}
              </div>
              {/* The card's video goes back to the stage and the map to the card. */}
              <button
                type="button"
                onClick={() => useCockpitViewStore.getState().setView("pilot")}
                aria-label={specText("cockpit.pip.swap")}
                title={specText("cockpit.pip.swap")}
                data-testid="video-card-swap"
                className="absolute right-[4px] top-[4px] z-[16] flex h-[48px] w-[48px] items-center justify-center rounded-md bg-hud-glass-strong text-hud-ink hover:bg-white/10"
              >
                <ArrowLeftRight className="h-[16px] w-[16px]" aria-hidden />
              </button>
            </div>
          ) : (
            <BottomLeftCardView slots={slots} m={m} />
          )}
        </Zone>
      ) : null}
      {L.bottomRow && slots.bottomRow ? (
        <Zone r={L.bottomRow} scale={s} className={Z.zoneChrome} zone="bottom_row">
          {slots.bottomRow(L.bottomRowShort)}
        </Zone>
      ) : null}
      <Zone r={L.skillPicker} scale={s} className={cn("flex items-end justify-end", Z.zoneChrome)} zone="skill_picker">
        {slots.skillPicker}
      </Zone>
      {slots.onscreenSticks}
    </div>
  );
}

export function PortraitGrid({ slots }: { slots: FeedSlots }) {
  return (
    <div className="pointer-events-none absolute inset-[8px] flex flex-col gap-[8px]">
      <div className="pointer-events-auto relative aspect-video w-full shrink-0 overflow-hidden rounded-[var(--fs-radius)]">
        <MainVideo video={slots.video} layers={slots.layers} />
        <div className="pointer-events-none absolute inset-0">{slots.hud}</div>
        <div className={cn("absolute inset-x-[4px] top-[4px] h-[48px]", Z.topBar)} data-zone-rect="top_bar">
          {slots.topBar}
        </div>
        <div className={cn("pointer-events-none absolute inset-x-0 top-[60px] flex flex-col items-center gap-[6px]", Z.alerts)}>
          {slots.overlays}
        </div>
        {slots.pip ? <div className={cn("absolute bottom-[4px] right-[4px]", Z.pip)}>{slots.pip}</div> : null}
      </div>
      <div className="flex shrink-0 flex-col items-center gap-[6px]">{slots.alerts}</div>
      {slots.followStrip ? <div className="h-[56px] shrink-0">{slots.followStrip(0)}</div> : null}
      <div className={cn("flex shrink-0 items-center justify-between gap-[8px]", Z.zoneChrome)}>
        {slots.leftColumn({ row: true, labels: false })}
        {slots.skillPicker}
      </div>
      {slots.bottomRow ? <div className="flex h-[44px] shrink-0 justify-center">{slots.bottomRow(true)}</div> : null}
      {slots.ack}
      <div className="chrome-panel relative min-h-0 flex-1 overflow-hidden">
        {slots.map?.("h-full w-full")}
        {slots.cards.obstacle_radar ? (
          <div className={cn("absolute bottom-[8px] right-[8px] h-[96px] w-[96px]", Z.zones)}>
            {slots.cards.obstacle_radar("h-full w-full")}
          </div>
        ) : null}
      </div>
    </div>
  );
}
