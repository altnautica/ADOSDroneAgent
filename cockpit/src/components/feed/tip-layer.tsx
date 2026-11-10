// Contextual tips on the flight screen (cockpit spec `tips.json`): one small
// panel at a time beside its anchor, on the ground only, never over STOP, the
// left column or the flight-state chip (nor any other `never_cover_zones`
// entry), never while a sheet, a menu or the controller tour is up. No scrim:
// input outside the tip passes through, and tapping the tip, its anchor or
// its button, pressing any controller button, or arming dismisses it for
// good. Unattended, it hides after `auto_dismiss_ms` and shows again on a
// later open. Mounted once by the Feed; the kiosk shows tips too.

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";

import { useTourStage } from "@/components/controls/tour-stage";
import { pilotFresh } from "@/shared/hud/pilot-fresh";
import { skillVisibleFor } from "@/lib/skills";
import tipsJson from "@/shared/cockpit-spec/tips.json";
import { gotoAdvertised } from "@/shared/hud/fly-here-offer";
import { specText } from "@/shared/hud/spec-text";
import { placeTip, TIPS, type TipPlacement, type TipRect } from "@/shared/hud/tips";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useConfirmStore } from "@/stores/confirm-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useInputStore } from "@/stores/input-store";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { alertBannerVisible, useTipsStore, type TipsScreen } from "@/stores/tips-store";

/** Each spec anchor and zone, as the element that carries it on the Feed. */
const TIP_SELECTORS: Readonly<Record<string, string>> = {
  status_pill: "[data-zone=status_pill]",
  flight_state_chip: "[data-testid=flight-state-chip]",
  layer_button: "[data-zone=layer_button]",
  stop: "[data-zone=stop]",
  skill_picker_chip: "[data-testid=skill-picker-chip]",
  bottom_left_card: "[data-zone=bottom_left_card]",
  gimbal_pad: "[data-zone=gimbal_pad]",
  left_column: "[data-zone=left_column]",
  top_bar: "[data-zone=top_bar]",
  alert_stack: "[data-zone=alert_stack]",
};

/** Each anchor once. */
const ANCHORS: readonly string[] = TIPS.map((t) => t.anchor).filter((a, i, all) => all.indexOf(a) === i);
const NEVER_COVER: readonly string[] = tipsJson.rules.never_cover_zones;

/** How often the layer re-reads the screen, ms. */
const TICK_MS = 250;
/** The tip's width before it is measured, and its text size, unscaled px. */
const TIP_MAX_W_PX = 260;
const TIP_TEXT_PX = 13;

/** The element's rect when it is laid out and on screen, else null. */
function visibleRect(selector: string): TipRect | null {
  const el = document.querySelector(selector);
  return el ? elementRect(el) : null;
}

/** The element's rect when it is laid out and on screen, else null. */
function elementRect(el: Element): TipRect | null {
  const r = el.getBoundingClientRect();
  if (r.width <= 0 || r.height <= 0) return null;
  if (r.right <= 0 || r.bottom <= 0 || r.left >= window.innerWidth || r.top >= window.innerHeight) return null;
  if (getComputedStyle(el).visibility === "hidden") return null;
  return { x: r.left, y: r.top, w: r.width, h: r.height };
}

/** Beside `anchor` in the window, clear of every `never_cover_zones` rect and
 *  of the top bar, so a tip never hides the bar's own readouts. */
function placeFor(anchor: TipRect, size: { w: number; h: number }): TipPlacement | null {
  const avoid = [...NEVER_COVER, "top_bar"].flatMap((z) => {
    const r = TIP_SELECTORS[z] ? visibleRect(TIP_SELECTORS[z]) : null;
    return r ? [r] : [];
  });
  return placeTip(anchor, size, { x: 0, y: 0, w: window.innerWidth, h: window.innerHeight }, avoid);
}

interface Shown {
  tipId: string;
  textKey: string;
  anchor: string;
  at: TipPlacement | null;
}

export function TipLayer({ gimbalAvailable, scale }: { gimbalAvailable: boolean; scale: number }) {
  const tourVisible = useTourStage() !== "none";
  const extensions = useExtensionsStore((s) => s.extensions);
  const vehicleClass = useNavStreamStore((s) => s.pilot?.data.vehicleClass ?? null);
  const skillsAvailable = useMemo(
    () => extensions.some((e) => e.skills.some((s) => skillVisibleFor(s, vehicleClass))),
    [extensions, vehicleClass],
  );
  const [shown, setShown] = useState<Shown | null>(null);
  const tipRef = useRef<HTMLDivElement>(null);
  const size = useRef({ w: TIP_MAX_W_PX * scale, h: 72 * scale });
  const live = useRef({ tourVisible, skillsAvailable, gimbalAvailable });
  useEffect(() => {
    live.current = { tourVisible, skillsAvailable, gimbalAvailable };
  });

  useEffect(() => {
    const store = useTipsStore.getState();
    store.flightScreenOpened(Date.now());
    const tick = () => {
      const now = Date.now();
      const p = useNavStreamStore.getState().pilot;
      const pilot = pilotFresh(p?.at, now) ? (p?.data ?? null) : null;
      const telemetryArmed = useFlightStore.getState().telemetry?.armed === true;
      const nav = useNavStore.getState();
      const anchorsVisible: Record<string, boolean> = {};
      const rects: Record<string, TipRect> = {};
      for (const a of ANCHORS) {
        const r = visibleRect(TIP_SELECTORS[a] ?? "");
        anchorsVisible[a] = r !== null;
        if (r) rects[a] = r;
      }
      const screen: TipsScreen = {
        armed: pilot ? pilot.armed : telemetryArmed,
        airborne: pilot ? pilot.armed && !pilot.landed : telemetryArmed,
        flightScreen: activeScreenId(nav) === "feed" && nav.detailStack.length === 0,
        sheetOrMenuOpen:
          useCockpitUiStore.getState().sheet !== null ||
          nav.quickMenuOpen ||
          useInputStore.getState().pickerOpen ||
          useConfirmStore.getState().pending !== null ||
          document.querySelector("[role=dialog]") !== null,
        tourVisible: live.current.tourVisible,
        alertBannerVisible: alertBannerVisible(document, (el) => elementRect(el) !== null),
        skillsAvailable: live.current.skillsAvailable,
        gotoOffered: gotoAdvertised(pilot),
        gimbalAvailable: live.current.gimbalAvailable,
        anchorsVisible,
      };
      const d = useTipsStore.getState().tick(screen, now);
      if (d.kind !== "show") {
        setShown((cur) => (cur ? null : cur));
        return;
      }
      const at = placeFor(rects[d.anchor], size.current);
      setShown((cur) =>
        cur && cur.tipId === d.tipId && cur.at?.x === at?.x && cur.at?.y === at?.y ? cur : { tipId: d.tipId, textKey: d.textKey, anchor: d.anchor, at },
      );
    };
    tick();
    const id = window.setInterval(tick, TICK_MS);
    return () => {
      window.clearInterval(id);
      useTipsStore.getState().flightScreenClosed();
    };
  }, []);

  // Measure the tip once drawn and, before it paints, place it again with its
  // real size.
  useLayoutEffect(() => {
    const r = tipRef.current?.getBoundingClientRect();
    if (!r || r.width <= 0 || !shown) return;
    if (Math.abs(r.height - size.current.h) < 0.5 && Math.abs(r.width - size.current.w) < 0.5) return;
    size.current = { w: r.width, h: r.height };
    const anchorRect = visibleRect(TIP_SELECTORS[shown.anchor] ?? "");
    if (anchorRect) setShown({ ...shown, at: placeFor(anchorRect, size.current) });
  }, [shown]);

  // Tapping the tip or its anchor dismisses it; the tap still reaches what
  // it landed on.
  const anchor = shown?.anchor ?? null;
  useEffect(() => {
    if (!anchor) return;
    const onDown = (e: PointerEvent) => {
      const t = e.target instanceof Element ? e.target : null;
      if (!t) return;
      const anchorEl = document.querySelector(TIP_SELECTORS[anchor] ?? "");
      if (tipRef.current?.contains(t) || anchorEl?.contains(t)) {
        useTipsStore.getState().dismissShown(Date.now());
        setShown(null);
      }
    };
    document.addEventListener("pointerdown", onDown, true);
    return () => document.removeEventListener("pointerdown", onDown, true);
  }, [anchor]);

  // Any controller button dismisses it.
  useEffect(() => {
    if (!anchor) return;
    return useInputStore.subscribe((st, prev) => {
      const now = st.pad?.pressed ?? [];
      const was = prev.pad?.pressed ?? [];
      if (now.some((p, i) => p && !was[i])) {
        useTipsStore.getState().dismissShown(Date.now());
        setShown(null);
      }
    });
  }, [anchor]);

  if (!shown) return null;
  return (
    <div
      ref={tipRef}
      role="status"
      aria-live="polite"
      data-testid="cockpit-tip"
      data-tip={shown.tipId}
      className="chrome-panel pointer-events-auto fixed z-[46] flex flex-col items-start"
      style={{
        left: shown.at?.x ?? 0,
        top: shown.at?.y ?? 0,
        width: TIP_MAX_W_PX * scale,
        gap: 8 * scale,
        padding: 10 * scale,
        fontSize: TIP_TEXT_PX * scale,
        lineHeight: 1.35,
        visibility: shown.at ? "visible" : "hidden",
      }}
    >
      <p>{specText(shown.textKey)}</p>
      <button
        type="button"
        data-testid="cockpit-tip-dismiss"
        className="self-end rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] font-semibold"
        style={{ minHeight: 36 * scale, padding: `0 ${12 * scale}px` }}
      >
        {specText("tips.dismiss")}
      </button>
    </div>
  );
}
