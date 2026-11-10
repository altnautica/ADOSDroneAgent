import { useRef, type KeyboardEvent } from "react";
import { Crosshair } from "lucide-react";

import { useGimbalPadStore } from "@/hooks/use-gimbal-rate";
import { runCameraAction } from "@/lib/camera-actions";
import { FLIGHT_PX } from "@/lib/flight-layout";
import { GIMBAL_KEY_DPS, GIMBAL_MAX_DPS } from "@/lib/gimbal-rate";
import { pressOrExplain } from "@/lib/zone-press";
import { specText } from "@/shared/hud/spec-text";

import { useCameraReason } from "./camera-reason";

const ARROW_RATE: Readonly<Record<string, [number, number]>> = {
  ArrowUp: [GIMBAL_KEY_DPS, 0],
  ArrowDown: [-GIMBAL_KEY_DPS, 0],
  ArrowLeft: [0, -GIMBAL_KEY_DPS],
  ArrowRight: [0, GIMBAL_KEY_DPS],
};

/** The gimbal pad: drag from the centre to aim (rate grows with the drag);
 *  release stops it; a tap on the centre recentres. With the pad focused the
 *  arrow keys aim at a fixed rate while held and Enter recentres; Space is
 *  left to the rest of the cockpit. Shown only where the vehicle takes
 *  gimbal rates. */
export function GimbalPad() {
  const setRate = useGimbalPadStore((s) => s.setRate);
  const pitch = useGimbalPadStore((s) => s.pitch);
  const yaw = useGimbalPadStore((s) => s.yaw);
  const recenterReason = useCameraReason("recenter");
  const recenter = () => pressOrExplain(recenterReason, () => void runCameraAction("recenter"));
  const origin = useRef<{ x: number; y: number; r: number; captured: boolean } | null>(null);
  const keys = useRef(new Set<string>());
  const size = FLIGHT_PX.gimbal;
  const knob = FLIGHT_PX.knob;
  const travel = (size - knob) / 2;
  const kx = (yaw / GIMBAL_MAX_DPS) * travel;
  const ky = (-pitch / GIMBAL_MAX_DPS) * travel;
  const release = () => {
    origin.current = null;
    setRate(0, 0);
  };
  const keyRate = () => {
    let p = 0;
    let y = 0;
    for (const k of keys.current) {
      p += ARROW_RATE[k][0];
      y += ARROW_RATE[k][1];
    }
    setRate(p, y);
  };
  const recentreLabel = specText("cockpit.gimbal.recentre");
  return (
    <div
      className="chrome-panel relative shrink-0 touch-none !rounded-full"
      style={{ width: size, height: size }}
      role="group"
      tabIndex={0}
      aria-label={specText("cockpit.gimbal_pad")}
      title={specText("cockpit.gimbal_pad.hint")}
      data-zone="gimbal_pad"
      onKeyDown={(e: KeyboardEvent<HTMLDivElement>) => {
        if (e.target !== e.currentTarget) return;
        if (e.key === "Enter") {
          e.preventDefault();
          recenter();
          return;
        }
        if (!(e.key in ARROW_RATE)) return;
        e.preventDefault();
        if (keys.current.has(e.key)) return;
        keys.current.add(e.key);
        keyRate();
      }}
      onKeyUp={(e) => {
        if (!keys.current.delete(e.key)) return;
        keyRate();
      }}
      onBlur={() => {
        if (keys.current.size === 0) return;
        keys.current.clear();
        setRate(0, 0);
      }}
      onPointerDown={(e) => {
        const r = e.currentTarget.getBoundingClientRect();
        // A press on the recentre button is not captured: capture retargets
        // the mouse click to the pad and the button would never run. Any
        // other press captures at once, so the release always reaches the pad.
        const onKnob = (e.target as Element).closest("button") !== null;
        if (!onKnob) e.currentTarget.setPointerCapture?.(e.pointerId);
        origin.current = { x: r.left + r.width / 2, y: r.top + r.height / 2, r: r.width / 2, captured: !onKnob };
      }}
      onPointerMove={(e) => {
        const o = origin.current;
        if (!o) return;
        // No button down: the release happened elsewhere, so the drag is over.
        if (e.buttons === 0) {
          release();
          return;
        }
        const dx = Math.max(-1, Math.min(1, (e.clientX - o.x) / o.r));
        const dy = Math.max(-1, Math.min(1, (e.clientY - o.y) / o.r));
        // A small dead zone round the centre so a tap does not nudge.
        const shape = (v: number) => (Math.abs(v) < 0.15 ? 0 : Math.sign(v) * ((Math.abs(v) - 0.15) / 0.85) ** 1.5);
        const ratePitch = -shape(dy) * GIMBAL_MAX_DPS;
        const rateYaw = shape(dx) * GIMBAL_MAX_DPS;
        if (!o.captured && (ratePitch !== 0 || rateYaw !== 0)) {
          o.captured = true;
          e.currentTarget.setPointerCapture?.(e.pointerId);
        }
        setRate(ratePitch, rateYaw);
      }}
      onPointerUp={release}
      onPointerCancel={release}
      onLostPointerCapture={release}
    >
      {[0, 90, 180, 270].map((deg) => (
        <span
          key={deg}
          className="absolute left-1/2 top-1/2 h-0 w-0 border-x-[5px] border-b-[6px] border-x-transparent border-b-[var(--fs-text-dim)]"
          style={{ transform: `translate(-50%, -50%) rotate(${deg}deg) translateY(${-(size / 2 - 10)}px)` }}
          aria-hidden
        />
      ))}
      {/* The knob is drawn 36 px; its button is the 48 px touch floor. */}
      <button
        type="button"
        tabIndex={-1}
        onClick={recenter}
        aria-disabled={recenterReason ? true : undefined}
        aria-label={recenterReason ? `${recentreLabel}: ${recenterReason}` : recentreLabel}
        title={recenterReason ?? recentreLabel}
        className="group absolute left-1/2 top-1/2 flex items-center justify-center rounded-full transition-transform duration-100 ease-out"
        style={{ width: FLIGHT_PX.knobHit, height: FLIGHT_PX.knobHit, transform: `translate(calc(-50% + ${kx}px), calc(-50% + ${ky}px))` }}
      >
        <span
          className="flex items-center justify-center rounded-full border border-[var(--fs-border)] bg-white/15 group-aria-disabled:opacity-60"
          style={{ width: knob, height: knob }}
        >
          <Crosshair className="h-[16px] w-[16px]" aria-hidden />
        </span>
      </button>
    </div>
  );
}
