// The controller tour on the first controller connect: three cards (sticks,
// safety buttons, skills) in a small non-modal card, shown once per device
// (`tourSeen`). No scrim: the cockpit stays visible and usable around it, and
// the card sits clear of STOP and the left column. While the browser has
// announced a controller it has not yet reported (the Gamepad API waits for a
// button press), the card asks for a button press first. It shows only on
// the ground, on every screen, and waits while the vehicle is in the air.
//
// The pad drives it: D-pad moves between Next and Skip, Cross presses the
// focused one, Circle closes it. The pad is captured for the card only while
// the vehicle is known to be on the ground and disarmed, so Cross does not
// also start a skill; with the arm state unknown the card shows without
// capturing, and the safety bindings stay live.

import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { useTourCapturesPad, useTourStage } from "@/components/controls/tour-stage";
import { useWidthClass } from "@/hooks/use-width-class";
import { gridMeasures } from "@/lib/cockpit-layout";
import { chromeStyle } from "@/lib/chrome-tokens";
import { TOUR_BUTTONS, TOUR_CARDS, tourCardRect, tourPadCommand, tourStep, type Rect, type TourCursor } from "@/lib/controller-tour";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore } from "@/stores/input-store";

/** Card width and its gap above the bottom edge, unscaled px. */
const CARD_MAX_W_PX = 360;
const CARD_BOTTOM_PX = 120;

/** The zones the card stays clear of; the alerts are the notices and the
 *  alert stack together. */
const TOP_BAR = "[data-zone=top_bar]";
const ALERTS = ['[data-testid="feed-notices"]', "[data-zone=alert_stack]"];

function zoneRect(selector: string): Rect | null {
  const el = document.querySelector(selector);
  if (!el) return null;
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0 ? { x: r.left, y: r.top, w: r.width, h: r.height } : null;
}

/** The box around every shown alert zone, or null with none shown. */
function alertsRect(): Rect | null {
  const rects = ALERTS.map(zoneRect).filter((r): r is Rect => r !== null);
  if (rects.length === 0) return null;
  const x = Math.min(...rects.map((r) => r.x));
  const y = Math.min(...rects.map((r) => r.y));
  const right = Math.max(...rects.map((r) => r.x + r.w));
  const bottom = Math.max(...rects.map((r) => r.y + r.h));
  return { x, y, w: right - x, h: bottom - y };
}

export function ControllerTour() {
  const stage = useTourStage();
  const markSeen = useControlsStore((s) => s.markTourSeen);
  const captures = useTourCapturesPad();
  const { widthClass } = useWidthClass();
  const scale = gridMeasures(widthClass, false).scale;
  const [cursor, setCursor] = useState<TourCursor>({ card: 0, focus: 0 });
  const cardRef = useRef<HTMLDivElement>(null);
  const [rect, setRect] = useState<Rect | null>(null);

  // A replay starts from the first card.
  useEffect(() => {
    if (stage === "none") setCursor({ card: 0, focus: 0 });
  }, [stage]);

  const apply = (cmd: Parameters<typeof tourStep>[1]) => {
    const next = tourStep(cursor, cmd);
    if (next.done) markSeen();
    else setCursor({ card: next.card, focus: next.focus });
  };
  const applyRef = useRef(apply);
  useEffect(() => {
    applyRef.current = apply;
  });

  // The pad drives the card: capture it (no flight action, no menu move)
  // while the card is up on a vehicle known to be grounded and disarmed, and
  // read its button edges.
  const drive = stage === "tour" && captures;
  useEffect(() => {
    if (!drive) return;
    const input = useInputStore.getState();
    const owned = !input.capture;
    if (owned) input.setCapture(true);
    const off = useInputStore.subscribe((st, prev) => {
      const now = st.pad?.pressed ?? [];
      const was = prev.pad?.pressed ?? [];
      for (let i = 0; i < now.length; i += 1) {
        if (!now[i] || was[i]) continue;
        const cmd = tourPadCommand(i);
        if (cmd) {
          applyRef.current(cmd);
          return;
        }
      }
    });
    return () => {
      off();
      if (owned) useInputStore.getState().setCapture(false);
    };
  }, [drive]);

  // Placed after layout and on resize: clear of the top bar, the alert stack,
  // STOP and the left column; on a short window the body scrolls.
  useLayoutEffect(() => {
    if (stage === "none") return;
    const place = () => {
      const el = cardRef.current;
      const box = el?.getBoundingClientRect().height ?? 0;
      // The content's own height, whatever the cap the last placement set.
      const natural = el && el.clientHeight > 0 ? box * (el.scrollHeight / el.clientHeight) : box;
      setRect(
        tourCardRect(
          window.innerWidth,
          window.innerHeight,
          {
            leftColumn: zoneRect("[data-zone=left_column]"),
            stop: zoneRect("[data-zone=stop]"),
            topBar: zoneRect(TOP_BAR),
            alertStack: alertsRect(),
          },
          CARD_MAX_W_PX * scale,
          natural,
          CARD_BOTTOM_PX * scale,
        ),
      );
    };
    place();
    window.addEventListener("resize", place);
    // An alert or a notice coming up moves the card down.
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(place);
    for (const sel of [TOP_BAR, ...ALERTS]) {
      const el = document.querySelector(sel);
      if (el) observer?.observe(el);
    }
    return () => {
      window.removeEventListener("resize", place);
      observer?.disconnect();
    };
  }, [stage, cursor.card, scale]);

  if (stage === "none") return null;
  const id = TOUR_CARDS[cursor.card] ?? TOUR_CARDS[0];
  const last = cursor.card === TOUR_CARDS.length - 1;
  const button = (b: (typeof TOUR_BUTTONS)[number], i: number) => (
    <button
      key={b}
      type="button"
      data-testid={`controls-tour-${b}`}
      onClick={() => apply(b)}
      className={cn(
        "min-h-[44px] flex-1 rounded-[var(--fs-radius-sm)] border px-[12px] text-[13px] font-semibold",
        b === "advance" ? "border-primary bg-primary/25" : "border-[var(--fs-border)]",
        drive && cursor.focus === i && "outline outline-2 outline-offset-2 outline-primary",
      )}
    >
      {b === "skip" ? specText("controls.tour.skip") : last ? specText("controls.tour.done") : specText("controls.tour.next")}
    </button>
  );

  return (
    <div
      ref={cardRef}
      role="region"
      aria-label={specText("controls.tour.title")}
      data-testid="controls-tour"
      className="chrome-panel pointer-events-auto fixed z-[48] flex flex-col gap-[8px] overflow-y-auto p-[12px]"
      style={{
        ...chromeStyle(),
        left: (rect?.x ?? 0) / scale,
        top: (rect?.y ?? 0) / scale,
        width: (rect?.w ?? CARD_MAX_W_PX * scale) / scale,
        maxHeight: rect ? rect.h / scale : undefined,
        visibility: rect ? "visible" : "hidden",
        zoom: scale === 1 ? undefined : scale,
      }}
    >
      {stage === "press" ? (
        <p className="text-[14px]">{specText("controls.press_any_button")}</p>
      ) : (
        <>
          <div className="chrome-label uppercase tracking-wide">{specText("controls.tour.title")}</div>
          <h3 className="text-[15px] font-semibold">{specText(`controls.tour.${id}.title`)}</h3>
          <p className="text-[13px] leading-[18px]">{specText(`controls.tour.${id}.body`)}</p>
          <div className="flex gap-[8px]">{TOUR_BUTTONS.map(button)}</div>
        </>
      )}
    </div>
  );
}
