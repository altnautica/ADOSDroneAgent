// The shared overlay frame under every sheet, dialog and menu: a portal over
// the cockpit (CS-1 L4), focus trapped inside and returned on close, and the
// panel buttons / gamepad moving focus between its controls through the
// command stack, so nothing underneath moves while it is open.
//
// On the flight screen an overlay starts below the top bar (`layout.json`
// `sheets.top_inset`), and its scrim leaves the top bar uncovered, so STOP
// (`sheets.never_cover_zones`) takes the first press with any sheet open.
//
// One overlay at a time: opening one closes the one open before it (an
// overlay opened from inside another, a numpad over its sheet, stacks on it
// instead). Escape, a press anywhere outside the newest overlay and the
// controller back close that overlay before anything else sees the press;
// a press on STOP passes through to STOP.

import {
  createContext,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
  type RefObject,
} from "react";
import { createPortal } from "react-dom";

import { pushCommandHandler } from "@/lib/command-stack";
import { Z } from "@/lib/cockpit-layout";
import { activateFocused, firstFocusable, moveFocus, trackFocusRing, trapTab } from "@/lib/focus-nav";
import { cn } from "@/lib/utils";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import { openOverlays as open, type OpenOverlay } from "@/stores/cockpit-ui-store";

export interface OverlayProps {
  onDismiss: () => void;
  /** The id of the element that titles the panel. */
  labelledBy?: string;
  /** Focused first; defaults to the panel's first control. */
  initialFocus?: RefObject<HTMLElement | null>;
  /** A dimmed scrim (sheets, dialogs) or a clear one (menus). */
  scrim?: "dim" | "clear";
  /** Positions the panel inside the layer below the top bar. */
  layerClassName?: string;
  panelClassName?: string;
  panelStyle?: CSSProperties;
  children: ReactNode;
}

interface SheetsRule {
  top_inset: "top_bar" | "none";
  never_cover_zones: readonly string[];
}

/** The sheets rule: the spec's, else the ledger rule it states (start below
 *  the top bar; never cover STOP). The spec file is ours and typed by its
 *  own schema test, so its value is taken as the rule. */
const SHEETS: SheetsRule =
  "sheets" in layoutJson ? (layoutJson.sheets as SheetsRule) : { top_inset: "top_bar", never_cover_zones: ["stop"] };

const PASS_THROUGH = SHEETS.never_cover_zones.map((z) => `[data-zone="${z}"]`).join(",");

const Nested = createContext(false);

/** Swallow the click that ends a press which closed an overlay, so the
 *  control under it does not act; the next press starts its own click. */
function swallowNextClick(): void {
  const click = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    done();
  };
  const done = () => {
    document.removeEventListener("click", click, true);
    document.removeEventListener("pointerdown", done, true);
  };
  document.addEventListener("click", click, true);
  // Added while this press is being dispatched, so it sees the next one only.
  document.addEventListener("pointerdown", done, true);
}

function onDocumentPointerDown(e: PointerEvent): void {
  const top = open[open.length - 1];
  const t = e.target as Element | null;
  if (!top || !t) return;
  if (top.panel.current?.contains(t)) return;
  if (PASS_THROUGH && t.closest?.(PASS_THROUGH)) return;
  e.preventDefault();
  e.stopPropagation();
  swallowNextClick();
  top.dismiss();
}

function onDocumentKeyDown(e: KeyboardEvent): void {
  if (e.key !== "Escape" || e.defaultPrevented) return;
  const top = open[open.length - 1];
  if (!top) return;
  e.preventDefault();
  e.stopPropagation();
  top.dismiss();
}

function register(entry: OpenOverlay): () => void {
  if (!entry.nested) {
    // One at a time: a new top-level overlay closes every one before it.
    for (const o of open.splice(0)) o.dismiss();
  }
  if (open.length === 0) {
    document.addEventListener("pointerdown", onDocumentPointerDown, true);
    document.addEventListener("keydown", onDocumentKeyDown, true);
  }
  open.push(entry);
  return () => {
    const i = open.indexOf(entry);
    if (i >= 0) open.splice(i, 1);
    if (open.length === 0) {
      document.removeEventListener("pointerdown", onDocumentPointerDown, true);
      document.removeEventListener("keydown", onDocumentKeyDown, true);
    }
  };
}

/** The flight screen's top bar, or null off the flight screen. The flight
 *  layout (`lib/flight-layout.ts` `topBar`) places it for the current class,
 *  so its bottom edge is the class's top-bar height plus the edge inset. */
function topBar(): Element | null {
  return SHEETS.top_inset === "top_bar" ? document.querySelector('[data-zone="top_bar"]') : null;
}

/** The overlay's top inset, kept current: the top bar may mount after the
 *  overlay (a sheet opened on load) and changes size with the class. */
function useTopInset(): number {
  const [inset, setInset] = useState(0);
  useLayoutEffect(() => {
    let bar: Element | null = null;
    const measure = () => setInset(bar?.isConnected ? Math.ceil(bar.getBoundingClientRect().bottom) : 0);
    const ro = new ResizeObserver(measure);
    const find = () => {
      const next = topBar();
      if (next === bar) return;
      if (bar) ro.unobserve(bar);
      bar = next;
      if (bar) ro.observe(bar);
      measure();
    };
    find();
    // The flight screen mounts, leaves or re-renders its bar under an open
    // overlay; a cheap look twice a second follows it.
    const poll = window.setInterval(find, 500);
    window.addEventListener("resize", measure);
    return () => {
      window.clearInterval(poll);
      window.removeEventListener("resize", measure);
      ro.disconnect();
    };
  }, []);
  return inset;
}

export function Overlay({
  onDismiss,
  labelledBy,
  initialFocus,
  scrim = "dim",
  layerClassName,
  panelClassName,
  panelStyle,
  children,
}: OverlayProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const dismissRef = useRef(onDismiss);
  useEffect(() => {
    dismissRef.current = onDismiss;
  });
  const nested = useContext(Nested);
  const inset = useTopInset();

  // Registered before paint, so a frame never shows two sheets.
  useLayoutEffect(() => register({ dismiss: () => dismissRef.current(), panel: panelRef, nested }), [nested]);

  // Focus moves in on open and returns where it was on close; the focused
  // control carries the shared ring.
  useEffect(() => {
    const panel = panelRef.current;
    if (!panel) return;
    const previous = document.activeElement as HTMLElement | null;
    const stopRing = trackFocusRing(panel);
    (firstFocusable(initialFocus?.current ?? null) ?? firstFocusable(panel))?.focus();
    return () => {
      stopRing();
      previous?.focus?.();
    };
    // Focus is placed once, on open.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The controller commands drive this overlay while it is the newest one.
  useEffect(
    () =>
      pushCommandHandler((cmd) => {
        const panel = panelRef.current;
        if (!panel) return false;
        switch (cmd) {
          case "back":
            dismissRef.current();
            return true;
          case "prev":
            moveFocus(panel, -1);
            return true;
          case "next":
            moveFocus(panel, 1);
            return true;
          case "activate":
            activateFocused(panel);
            return true;
          case "cycle-tab":
            return true;
          case "quick-menu":
            return false;
        }
      }),
    [],
  );

  // The layer spans the window so a menu places itself in window coordinates;
  // its padding keeps the panels below the top bar, and only the scrim and the
  // panel take presses.
  return createPortal(
    <Nested.Provider value>
      <div
        className={cn("pointer-events-none fixed inset-0", Z.sheets, layerClassName)}
        style={{ paddingTop: inset }}
        data-overlay-inset={inset}
      >
        <div
          className={cn("pointer-events-auto absolute inset-x-0 bottom-0", scrim === "dim" ? "bg-scrim" : "bg-transparent")}
          style={{ top: inset }}
          data-testid="overlay-scrim"
          aria-hidden
        />
        <div
          ref={panelRef}
          role="dialog"
          aria-modal="true"
          aria-labelledby={labelledBy}
          className={cn("pointer-events-auto relative", panelClassName)}
          style={panelStyle}
          onKeyDown={(e) => {
            if (panelRef.current) trapTab(panelRef.current, e.nativeEvent);
          }}
        >
          {children}
        </div>
      </div>
    </Nested.Provider>,
    document.body,
  );
}
