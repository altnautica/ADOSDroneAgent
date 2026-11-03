// The two-step guard for dangerous actions (CS-6 `guarded`): the first tap
// arms a red guard for a few seconds; a hold inside that window fires. An
// armed guard that is not being held expires. Focus stays on the one control
// while it swaps between the guard and the hold, so a controller user is never
// dropped. Mirrors the Android GuardedButton.

import { useEffect, useRef, useState } from "react";
import type { LucideIcon } from "lucide-react";

import { useHold } from "@/hooks/use-hold";
import { GUARD_HOLD_MS, GUARD_WINDOW_MS } from "@/lib/skills";
import { cn } from "@/lib/utils";

export interface GuardedButtonProps {
  label: string;
  /** Shown while armed. */
  guardLabel?: string;
  onFired: () => void;
  icon?: LucideIcon;
  disabled?: boolean;
  guardWindowMs?: number;
  holdMs?: number;
  full?: boolean;
}

/** How often a lapsed guard re-checks whether it is still held. */
const HELD_RECHECK_MS = 100;

export function GuardedButton({
  label,
  guardLabel = "Hold to confirm",
  onFired,
  icon: Icon,
  disabled = false,
  guardWindowMs = GUARD_WINDOW_MS,
  holdMs = GUARD_HOLD_MS,
  full = false,
}: GuardedButtonProps) {
  const ref = useRef<HTMLButtonElement>(null);
  const [armed, setArmed] = useState(false);
  const hold = useHold({
    enabled: armed && !disabled,
    holdMs,
    onComplete: () => {
      setArmed(false);
      onFired();
    },
    targetRef: ref,
  });
  const holdingRef = useRef(false);
  useEffect(() => {
    holdingRef.current = hold.holding;
  });

  // The guard lapses after its window unless a hold is in progress then.
  useEffect(() => {
    if (!armed) return;
    let timer = setTimeout(function lapse() {
      if (holdingRef.current) timer = setTimeout(lapse, HELD_RECHECK_MS);
      else setArmed(false);
    }, guardWindowMs);
    return () => clearTimeout(timer);
  }, [armed, guardWindowMs]);

  const live = armed && !disabled;
  return (
    <button
      ref={ref}
      type="button"
      disabled={disabled}
      onClick={() => {
        if (!armed) setArmed(true);
      }}
      {...(live ? hold.pointerHandlers : {})}
      {...hold.keyHandlers}
      aria-live="polite"
      className={cn(
        "touch-target relative flex touch-none select-none items-center justify-center gap-[0.4rem] overflow-hidden rounded-lg border px-[0.9rem] text-[0.85rem] font-semibold transition-colors duration-quick disabled:opacity-50",
        full && "w-full",
        live ? "border-err bg-err/25 text-err" : "border-border bg-err/15 text-err hover:bg-err/25",
      )}
    >
      {live ? (
        <span className="absolute inset-y-0 left-0 bg-err/45" style={{ width: `${hold.progress * 100}%` }} aria-hidden />
      ) : null}
      {Icon ? <Icon className="relative h-[1.1rem] w-[1.1rem]" aria-hidden /> : null}
      <span className="relative">{live ? guardLabel : label}</span>
    </button>
  );
}
