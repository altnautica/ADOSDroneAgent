// Slide to confirm (CS-6 `slide`): drag the thumb from the start of the track
// to its end; releasing early snaps it back, and a tap anywhere never
// completes it. With focus, holding Enter, the gamepad's A, or a long press of
// the panel's select drives the thumb across instead. Mirrors the Android
// SlideToConfirm.

import { useRef, useState } from "react";
import { ArrowRight } from "lucide-react";

import { useHold } from "@/hooks/use-hold";
import { SLIDE_HOLD_MS } from "@/lib/skills";
import { SLIDE_START_MAX, slideCompletes } from "@/lib/slide";
import { cn } from "@/lib/utils";

export interface SlideToConfirmProps {
  label: string;
  onConfirmed: () => void;
  disabled?: boolean;
  keyHoldMs?: number;
  /** Progress of a hold the caller times itself (0..1). While set, the track
   *  shows it and leaves its own keyboard, gamepad and panel holds off. */
  externalProgress?: number;
}

export function SlideToConfirm({
  label,
  onConfirmed,
  disabled = false,
  keyHoldMs = SLIDE_HOLD_MS,
  externalProgress,
}: SlideToConfirmProps) {
  const trackRef = useRef<HTMLDivElement>(null);
  const [frac, setFrac] = useState(0);
  const startFrac = useRef<number | null>(null);
  const hold = useHold({
    enabled: !disabled && externalProgress === undefined,
    holdMs: keyHoldMs,
    onComplete: onConfirmed,
    targetRef: trackRef,
  });

  const fracAt = (clientX: number) => {
    const rect = trackRef.current?.getBoundingClientRect();
    if (!rect || rect.width <= 0) return 0;
    return Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
  };
  const reset = () => {
    startFrac.current = null;
    setFrac(0);
  };
  const shown = Math.max(frac, externalProgress ?? hold.progress);

  return (
    <div
      ref={trackRef}
      role="slider"
      tabIndex={disabled ? -1 : 0}
      aria-label={label}
      aria-disabled={disabled || undefined}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(shown * 100)}
      {...hold.keyHandlers}
      className={cn(
        "relative h-[max(3.4rem,56px)] w-full touch-none select-none rounded-xl border border-border bg-muted",
        disabled && "opacity-50",
      )}
      onPointerDown={(e) => {
        if (disabled) return;
        // A drag counts only when it starts on the thumb; pressing never confirms.
        const f = fracAt(e.clientX);
        if (f >= SLIDE_START_MAX) return;
        startFrac.current = f;
        e.currentTarget.setPointerCapture?.(e.pointerId);
      }}
      onPointerMove={(e) => {
        if (startFrac.current === null) return;
        const f = fracAt(e.clientX);
        setFrac(f);
        if (slideCompletes(startFrac.current, f)) {
          reset();
          onConfirmed();
        }
      }}
      onPointerUp={reset}
      onPointerCancel={reset}
    >
      <span className="absolute inset-0 flex items-center justify-center pl-[3.4rem] pr-[1rem] text-[0.85rem] text-muted-foreground">
        {label}
      </span>
      <span
        className="absolute inset-y-[0.3rem] flex w-[3rem] items-center justify-center rounded-lg bg-primary text-primary-foreground"
        style={{ left: `calc(0.3rem + ${shown} * (100% - 3.6rem))` }}
        aria-hidden
      >
        <ArrowRight className="h-[1.3rem] w-[1.3rem]" />
      </span>
    </div>
  );
}
