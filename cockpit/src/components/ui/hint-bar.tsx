// The controller hints for the surface in front of the operator: which button
// does what, right-aligned along the bottom edge. Shown only while a gamepad
// or the panel buttons were the last input; it repeats actions reachable by
// touch, so it is hidden from assistive technology.

import { glyphFor } from "@/lib/controller-glyphs";
import { cn } from "@/lib/utils";
import type { NavCommand } from "@/nav/navigator";
import { isControllerSource, useInputStore } from "@/stores/input-store";

export interface Hint {
  command: NavCommand;
  label: string;
}

export function HintBar({ hints, className }: { hints: Hint[]; className?: string }) {
  const source = useInputStore((s) => s.source);
  if (!isControllerSource(source) || hints.length === 0) return null;
  return (
    <div
      aria-hidden
      className={cn(
        "flex h-[2rem] shrink-0 items-center justify-end gap-[1rem] bg-hud-glass px-[1rem] text-[0.75rem] text-muted-foreground",
        className,
      )}
    >
      {hints.map((h) => (
        <span key={`${h.command}:${h.label}`} className="flex items-center gap-[0.3rem]">
          <span className="rounded border border-border px-[0.3rem] font-mono text-surface-foreground">
            {glyphFor(source, h.command)}
          </span>
          <span>{h.label}</span>
        </span>
      ))}
    </div>
  );
}
