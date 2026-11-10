// On-screen sticks (Settings › Controls › On-screen sticks, default off): two
// thumb pads (`layout.json` `on_screen_sticks.pad_size`) in the bottom
// corners, shown in the spec's width classes (`on_screen_sticks.classes`) in
// landscape when no controller is connected, so a touch surface can fly the
// pilot in `ados` mode. They send the same `input` frames as a controller, at
// 50 Hz through the stick mode and the deadzone (the Controls setting, whose
// default and bounds are `ds4.json` `conditioning`), with the same stop rules:
// lifting both thumbs, hiding the tab or leaving `ados` mode sends `release`,
// and the pilot brakes to a hold.

import { useEffect, useRef, useState, type PointerEvent } from "react";

import { useWidthClass } from "@/hooks/use-width-class";
import { STICK_PAD_PX, onscreenSticksShown } from "@/lib/cockpit-layout";
import { radialDeadzone, pilotAxes, type Stick } from "@/lib/pad/conditioning";
import { STREAM_INTERVAL_MS } from "@/lib/pad/ds4-map";
import { PilotStreamer } from "@/lib/pad/pilot-stream";
import { releasePilotInput, sendPilotInput } from "@/lib/pilot-link";
import { cn } from "@/lib/utils";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore } from "@/stores/input-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const PAD_PX = STICK_PAD_PX;
const KNOB_PX = 44;

type Side = "l" | "r";

interface Touch {
  pointerId: number;
  stick: Stick;
}

export function OnscreenSticks() {
  const on = useControlsStore((s) => s.onscreenSticks);
  const padLive = useInputStore((s) => s.pad !== null);
  const { widthClass, portrait } = useWidthClass();
  const shown = onscreenSticksShown(on, padLive, portrait, widthClass);
  return shown ? <ThumbPads /> : null;
}

function ThumbPads() {
  const touches = useRef<Record<Side, Touch | null>>({ l: null, r: null });
  const [knobs, setKnobs] = useState<Record<Side, Stick>>({ l: [0, 0], r: [0, 0] });

  useEffect(() => {
    const streamer = new PilotStreamer({ send: sendPilotInput, release: releasePilotInput });
    const tick = () => {
      const { l, r } = touches.current;
      const controls = useControlsStore.getState();
      const dz = controls.deadzone;
      const sticks = {
        l: l ? radialDeadzone(l.stick[0], l.stick[1], dz) : ([0, 0] as Stick),
        r: r ? radialDeadzone(r.stick[0], r.stick[1], dz) : ([0, 0] as Stick),
      };
      const now = performance.now();
      streamer.tick({
        now,
        enabled: useNavStreamStore.getState().pilot?.data.controlMode === "ados",
        connected: l !== null || r !== null,
        hidden: document.visibilityState === "hidden",
        // The thumb pads exist only on the Feed.
        target: true,
        // A thumb on the glass is a live source whether or not it moves.
        sampleAt: now,
        axes: pilotAxes(sticks, controls.stickMode),
        buttons: 0,
      });
    };
    const onVisibility = () => tick();
    document.addEventListener("visibilitychange", onVisibility);
    const timer = setInterval(tick, STREAM_INTERVAL_MS);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      streamer.stop();
    };
  }, []);

  const move = (side: Side, e: PointerEvent<HTMLDivElement>) => {
    const t = touches.current[side];
    if (!t || t.pointerId !== e.pointerId) return;
    const rect = e.currentTarget.getBoundingClientRect();
    const half = rect.width / 2;
    let x = (e.clientX - (rect.left + half)) / half;
    let y = (e.clientY - (rect.top + half)) / half;
    const m = Math.hypot(x, y);
    if (m > 1) {
      x /= m;
      y /= m;
    }
    t.stick = [x, y];
    setKnobs((k) => ({ ...k, [side]: [x, y] }));
  };
  const down = (side: Side, e: PointerEvent<HTMLDivElement>) => {
    if (touches.current[side]) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    touches.current[side] = { pointerId: e.pointerId, stick: [0, 0] };
    move(side, e);
  };
  const up = (side: Side, e: PointerEvent<HTMLDivElement>) => {
    if (touches.current[side]?.pointerId !== e.pointerId) return;
    touches.current[side] = null;
    setKnobs((k) => ({ ...k, [side]: [0, 0] }));
  };

  return (
    <>
      {(["l", "r"] as const).map((side) => (
        <div
          key={side}
          role="slider"
          aria-label={side === "l" ? "Left on-screen stick" : "Right on-screen stick"}
          aria-valuenow={Math.round(knobs[side][1] * -100)}
          aria-valuemin={-100}
          aria-valuemax={100}
          data-testid={`onscreen-stick-${side}`}
          onPointerDown={(e) => down(side, e)}
          onPointerMove={(e) => move(side, e)}
          onPointerUp={(e) => up(side, e)}
          onPointerCancel={(e) => up(side, e)}
          style={{ width: PAD_PX, height: PAD_PX }}
          className={cn(
            "pointer-events-auto absolute bottom-[16px] z-30 touch-none select-none rounded-full border border-hud-hair bg-hud-glass backdrop-blur-hud",
            side === "l" ? "left-[16px]" : "right-[16px]",
          )}
        >
          <div
            aria-hidden
            className="absolute rounded-full border border-hud-hair-2 bg-hud-glass-strong"
            style={{
              width: KNOB_PX,
              height: KNOB_PX,
              left: PAD_PX / 2 - KNOB_PX / 2 + (knobs[side][0] * (PAD_PX - KNOB_PX)) / 2,
              top: PAD_PX / 2 - KNOB_PX / 2 + (knobs[side][1] * (PAD_PX - KNOB_PX)) / 2,
            }}
          />
        </div>
      ))}
    </>
  );
}
