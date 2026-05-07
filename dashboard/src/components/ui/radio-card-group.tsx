import { Check } from "lucide-react";
import { useRef, type KeyboardEvent } from "react";

import { cn } from "@/lib/utils";

interface RadioCardOption<T extends string> {
  value: T;
  label: string;
  description?: string;
  disabled?: boolean;
  badge?: string;
}

interface RadioCardGroupProps<T extends string> {
  value: T | null;
  onChange: (next: T) => void;
  options: ReadonlyArray<RadioCardOption<T>>;
  className?: string;
  columns?: 1 | 2 | 3;
}

export function RadioCardGroup<T extends string>({
  value,
  onChange,
  options,
  className,
  columns = 1,
}: RadioCardGroupProps<T>) {
  const cols =
    columns === 3 ? "lg:grid-cols-3" : columns === 2 ? "lg:grid-cols-2" : "";
  const buttonsRef = useRef<Array<HTMLButtonElement | null>>([]);

  // Roving-tabindex pattern. The selected option gets tabIndex=0; the
  // rest get -1. Arrow keys move focus + selection through the
  // enabled options (matching ARIA radiogroup spec).
  const enabledIndices = options
    .map((o, i) => ({ o, i }))
    .filter(({ o }) => !o.disabled)
    .map(({ i }) => i);

  function focusIndex(i: number) {
    const btn = buttonsRef.current[i];
    btn?.focus();
    const opt = options[i];
    if (opt && !opt.disabled) onChange(opt.value);
  }

  function onKeyDown(e: KeyboardEvent<HTMLButtonElement>, index: number) {
    if (
      e.key !== "ArrowRight" &&
      e.key !== "ArrowDown" &&
      e.key !== "ArrowLeft" &&
      e.key !== "ArrowUp" &&
      e.key !== "Home" &&
      e.key !== "End"
    ) {
      return;
    }
    e.preventDefault();
    if (enabledIndices.length === 0) return;
    const cur = enabledIndices.indexOf(index);
    let next: number;
    if (e.key === "Home") next = enabledIndices[0]!;
    else if (e.key === "End")
      next = enabledIndices[enabledIndices.length - 1]!;
    else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
      next = enabledIndices[(cur + 1) % enabledIndices.length]!;
    } else {
      next =
        enabledIndices[
          (cur - 1 + enabledIndices.length) % enabledIndices.length
        ]!;
    }
    focusIndex(next);
  }

  return (
    <div className={cn("grid gap-2", cols, className)} role="radiogroup">
      {options.map((opt, i) => {
        const selected = value === opt.value;
        const isFirstEnabled = enabledIndices[0] === i;
        const tabIndex = opt.disabled
          ? -1
          : selected || (value == null && isFirstEnabled)
            ? 0
            : -1;
        return (
          <button
            key={opt.value}
            ref={(el) => {
              buttonsRef.current[i] = el;
            }}
            type="button"
            role="radio"
            aria-checked={selected}
            tabIndex={tabIndex}
            disabled={opt.disabled}
            onClick={() => !opt.disabled && onChange(opt.value)}
            onKeyDown={(e) => onKeyDown(e, i)}
            className={cn(
              "text-left rounded-md border px-3 py-2.5 transition-colors focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring",
              selected
                ? "border-primary bg-primary/10 text-foreground"
                : "border-border bg-card hover:border-border/80 hover:bg-accent/30",
              opt.disabled && "opacity-50 cursor-not-allowed",
            )}
          >
            <div className="flex items-center justify-between gap-2 mb-0.5">
              <span className="font-medium text-sm">{opt.label}</span>
              <div className="flex items-center gap-2">
                {opt.badge && (
                  <span className="text-[10px] font-medium uppercase tracking-wider text-info">
                    {opt.badge}
                  </span>
                )}
                {selected && <Check className="h-3.5 w-3.5 text-primary" />}
              </div>
            </div>
            {opt.description && (
              <p className="text-xs text-muted-foreground leading-snug">
                {opt.description}
              </p>
            )}
          </button>
        );
      })}
    </div>
  );
}
