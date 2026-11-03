// A press-and-hold button (CS-6 `hold`): the action fires once the hold
// fills, from a pointer, Enter/Space, the gamepad's A or a panel long-press
// (`useHold`). Releasing early cancels; the fill shows the progress.

import { useRef, type ReactNode } from "react";

import { useHold } from "@/hooks/use-hold";
import { HOLD_MS } from "@/lib/skills";
import { cn } from "@/lib/utils";

export function HoldButton({
  children,
  onFired,
  disabled = false,
  holdMs = HOLD_MS,
  className,
  label,
}: {
  children: ReactNode;
  onFired: () => void;
  disabled?: boolean;
  holdMs?: number;
  className?: string;
  /** Accessible name; the visible text is the default. */
  label?: string;
}) {
  const ref = useRef<HTMLButtonElement>(null);
  const hold = useHold({ enabled: !disabled, holdMs, onComplete: onFired, targetRef: ref });
  return (
    <button
      ref={ref}
      type="button"
      disabled={disabled}
      aria-label={label}
      {...hold.pointerHandlers}
      {...hold.keyHandlers}
      className={cn(
        "touch-target relative flex touch-none select-none items-center justify-center overflow-hidden rounded-lg border border-border bg-surface px-[0.8rem] text-[0.85rem] font-semibold disabled:opacity-45",
        className,
      )}
    >
      <span className="absolute inset-y-0 left-0 bg-primary/35" style={{ width: `${hold.progress * 100}%` }} aria-hidden />
      <span className="relative">{children}</span>
    </button>
  );
}
