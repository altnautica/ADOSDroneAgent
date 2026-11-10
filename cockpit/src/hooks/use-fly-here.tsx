// Picking a point on any map (the bottom-left card, the full map view, the
// split pane, the local frame): a long-press (`layout.json`
// `map_long_press_ms`) or a right-click. In the air the confirm sheet shows
// the point with its distance and a hold (`holds_ms.fly_here`) sends it:
// the pilot's `goto` while the pilot accepts a surface goto, else a
// reposition to a flight controller in GUIDED (`flyHereOffered`). On the
// ground the same gesture opens the Launch sheet with the point as its go-to.
// Each map says how its points map to the world through `locate`.

import { useEffect, useRef, useState, type MouseEvent, type PointerEvent, type ReactNode } from "react";

import { Sheet } from "@/components/ui/sheet";
import { HoldButton } from "@/components/ui/hold-button";
import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import {
  FLY_HERE_HOLD_MS,
  FLY_HERE_PRESS_MS,
  FLY_HERE_SLOP_PX,
  flyHereArgs,
  flyHereFc,
  launchGoto,
  mapPickAction,
  sendFcReposition,
} from "@/lib/fly-here";
import { distanceMeters, type LatLon } from "@/lib/minimap-geometry";
import { pilotFresh } from "@/shared/hud/pilot-fresh";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";
import { sendPilotAction } from "@/lib/pilot-link";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useFlightStore } from "@/stores/flight-store";

/** The world under a map point: its lat/lon (null where the map has no
 *  geodetic frame) and metres north/east of the drone (null without one). */
export interface MapPick {
  ll: LatLon | null;
  fromDrone: { n: number; e: number } | null;
}

export interface MapPressHandlers {
  onPointerDown: (e: PointerEvent<Element>) => void;
  onPointerMove: (e: PointerEvent<Element>) => void;
  onPointerUp: () => void;
  onPointerCancel: () => void;
  onPointerLeave: () => void;
  onContextMenu: (e: MouseEvent<Element>) => void;
}

export interface FlyHere {
  /** The gesture handlers, or null while a pick would do nothing. */
  handlers: MapPressHandlers | null;
  /** The point awaiting the confirm hold. */
  target: LatLon | null;
  /** The confirm sheet (null while closed). */
  sheet: ReactNode;
}

/**
 * Long-press and right-click picking on a map. `locate` turns a client point
 * into the world point under it; `drone` is the drone's lat/lon for the
 * sheet's distance.
 */
export function useFlyHere(locate: (clientX: number, clientY: number) => MapPick | null, drone: LatLon | null): FlyHere {
  const pilot = usePilot();
  // Re-read twice a second so the gesture goes away once the pilot state is stale.
  const now = useNow(pilot ? 500 : null);
  const altRel = useFlightStore((s) => s.telemetry?.position?.alt_rel ?? null);
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const press = useRef<{ x: number; y: number; timer: number } | null>(null);
  const [target, setTarget] = useState<LatLon | null>(null);
  const [result, setResult] = useState<string | null>(null);
  const [sending, setSending] = useState(false);

  const freshPilot = pilot !== null && pilotFresh(pilot.at, now) ? pilot.data : null;
  const action = mapPickAction(pilot?.data ?? null, pilot ? now - pilot.at : Infinity, flyHereFc(telemetry ?? null, live, freshPilot));
  const flying = action === "pilot" || action === "fc";

  const pick = (clientX: number, clientY: number) => {
    const p = locate(clientX, clientY);
    if (!p) return;
    if (flying && p.ll) {
      setResult(null);
      setTarget(p.ll);
    } else if (action === "launch" && p.fromDrone) {
      useCockpitUiStore.getState().openLaunch(launchGoto(p.fromDrone));
    }
  };
  const cancel = () => {
    if (press.current) window.clearTimeout(press.current.timer);
    press.current = null;
  };
  // A press still running when the map goes away (a card switch) never fires.
  useEffect(
    () => () => {
      if (press.current) window.clearTimeout(press.current.timer);
      press.current = null;
    },
    [],
  );

  const handlers: MapPressHandlers | null =
    action === null
      ? null
      : {
          onPointerDown: (e) => {
            cancel();
            // The right button picks on its context menu event instead.
            if (e.pointerType === "mouse" && e.button !== 0) return;
            const { clientX, clientY } = e;
            press.current = {
              x: clientX,
              y: clientY,
              timer: window.setTimeout(() => {
                press.current = null;
                pick(clientX, clientY);
              }, FLY_HERE_PRESS_MS),
            };
          },
          onPointerMove: (e) => {
            const p = press.current;
            if (p && Math.hypot(e.clientX - p.x, e.clientY - p.y) > FLY_HERE_SLOP_PX) cancel();
          },
          onPointerUp: cancel,
          onPointerCancel: cancel,
          onPointerLeave: cancel,
          onContextMenu: (e) => {
            e.preventDefault();
            cancel();
            pick(e.clientX, e.clientY);
          },
        };

  const shown = flying ? target : null;
  const dist = shown && drone ? distanceMeters(drone, shown) : null;
  const args = shown ? flyHereArgs(shown, altRel) : null;
  const fly = () => {
    if (!args) return;
    setSending(true);
    const sent =
      action === "fc" ? sendFcReposition(args) : sendPilotAction("goto", args).then((r) => (r.ok ? null : refusalText(r.err)));
    void sent.then((refused) => {
      setSending(false);
      if (refused === null) setTarget(null);
      else setResult(refused);
    });
  };
  const line = "mt-[8px] text-[13px]";

  const sheet = shown ? (
    <Sheet
      title={specText("map.fly_here")}
      onDismiss={() => setTarget(null)}
      footer={
        <HoldButton onFired={fly} disabled={sending || !args} className="w-full" holdMs={FLY_HERE_HOLD_MS} label={specText("map.fly_here.hold")}>
          {specText("map.fly_here.hold")}
        </HoldButton>
      }
    >
      <dl className="grid grid-cols-[auto_1fr] gap-x-[16px] gap-y-[6px] text-[13px]">
        <dt className="text-muted-foreground">{specText("map.fly_here.point")}</dt>
        <dd className="font-mono">
          {shown.lat.toFixed(6)}, {shown.lon.toFixed(6)}
        </dd>
        <dt className="text-muted-foreground">{specText("map.fly_here.distance")}</dt>
        <dd className="font-mono">{dist === null ? "—" : specText("map.fly_here.distance_value", { dist: Math.round(dist) })}</dd>
        <dt className="text-muted-foreground">{specText("map.fly_here.height")}</dt>
        <dd className="font-mono">{args ? specText("map.fly_here.height_value", { alt: args.alt_m }) : "—"}</dd>
      </dl>
      <p className={`${line} text-hud-warn`}>{specText("map.fly_here.takes_over")}</p>
      {!args ? <p className={`${line} text-hud-warn`}>{specText("map.fly_here.needs_position")}</p> : null}
      {result ? <p className={`${line} text-hud-crit`}>{result}</p> : null}
    </Sheet>
  ) : null;

  return { handlers, target: shown, sheet };
}
